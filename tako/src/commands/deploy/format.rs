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
mod tests {
    use super::*;
    use crate::config::ServerEntry;

    #[test]
    fn format_runtime_summary_omits_empty_version() {
        assert_eq!(format_runtime_summary("bun", None), "Runtime: bun");
        assert_eq!(format_runtime_summary("bun", Some("")), "Runtime: bun");
    }

    #[test]
    fn format_runtime_summary_includes_version_when_present() {
        assert_eq!(
            format_runtime_summary("bun", Some("1.3.9")),
            "Runtime: bun (1.3.9)"
        );
    }

    #[test]
    fn format_servers_summary_joins_server_names() {
        let names = vec!["a".to_string(), "b".to_string()];
        assert_eq!(format_servers_summary(&names), "Servers: a, b");
    }

    #[test]
    fn should_use_unified_js_target_process_only_for_js_runtimes() {
        assert!(should_use_unified_js_target_process("bun"));
        assert!(should_use_unified_js_target_process("node"));
        assert!(!should_use_unified_js_target_process("go"));
    }

    #[test]
    fn deploy_progress_helpers_render_preparing_and_single_line_server_results() {
        let section = format_prepare_deploy_section("production");
        assert!(section.contains("Preparing deployment for"));
        assert!(section.contains("production"));

        let server = ServerEntry {
            host: "example.com".to_string(),
            port: 2222,
            description: None,
        };
        assert_eq!(
            format_server_deploy_success("prod", &server),
            "prod (tako@example.com:2222)"
        );
        assert_eq!(
            format_server_deploy_failure("prod", &server, "boom"),
            "prod (tako@example.com:2222): boom"
        );
    }

    #[test]
    fn deploy_overview_lines_include_primary_target_host_when_single_server() {
        let server = ServerEntry {
            host: "localhost".to_string(),
            port: 2222,
            description: None,
        };
        let lines =
            format_deploy_overview_lines("bun", "production", 1, Some(("testbed", &server)));
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("App") && lines[0].contains("bun"));
        assert!(lines[1].contains("Target") && lines[1].contains("testbed"));
        assert!(lines[2].contains("Host") && lines[2].contains("tako@localhost:2222"));
    }

    #[test]
    fn deploy_overview_lines_include_server_count_for_multi_target() {
        let lines = format_deploy_overview_lines("bun", "staging", 3, None);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("App") && lines[0].contains("bun"));
        assert!(lines[1].contains("Target") && lines[1].contains("3 servers"));
    }

    #[test]
    fn deploy_summary_lines_keep_urls_literal_and_contiguous() {
        let lines = format_deploy_summary_lines(
            "Release",
            "20260330",
            &[
                "app.test".to_string(),
                "app.test/bun".to_string(),
                "*.app.test".to_string(),
            ],
        );

        assert_eq!(
            lines,
            vec![
                SummaryLine {
                    label: "Release".to_string(),
                    value: "20260330".to_string(),
                },
                SummaryLine {
                    label: "Routes".to_string(),
                    value: "https://app.test".to_string(),
                },
                SummaryLine {
                    label: String::new(),
                    value: "https://app.test/bun".to_string(),
                },
                SummaryLine {
                    label: String::new(),
                    value: "https://*.app.test".to_string(),
                },
            ]
        );
    }

    #[test]
    fn deploy_summary_lines_support_non_url_primary_field() {
        let lines = format_deploy_summary_lines("App", "bun", &["app.test".to_string()]);

        assert_eq!(
            lines,
            vec![
                SummaryLine {
                    label: "App".to_string(),
                    value: "bun".to_string(),
                },
                SummaryLine {
                    label: "Routes".to_string(),
                    value: "https://app.test".to_string(),
                },
            ]
        );
    }

    #[test]
    fn format_deploy_main_message_omits_target_for_unified_process() {
        assert_eq!(
            format_deploy_main_message("dist/server/tako-entry.mjs", "linux-aarch64-musl", true),
            "Deploy main: dist/server/tako-entry.mjs"
        );
        assert_eq!(
            format_deploy_main_message("dist/server/tako-entry.mjs", "linux-aarch64-musl", false),
            "Deploy main: dist/server/tako-entry.mjs (artifact target: linux-aarch64-musl)"
        );
    }

    #[test]
    fn artifact_progress_helpers_render_build_and_packaging_steps() {
        assert_eq!(
            format_build_completed_message(Some("linux-aarch64-musl")),
            "Built for linux-aarch64-musl"
        );
        assert_eq!(
            format_prepare_artifact_message(Some("linux-aarch64-musl")),
            "Preparing artifact for linux-aarch64-musl"
        );
    }

    #[test]
    fn artifact_progress_helpers_render_shared_messages_without_target_label() {
        assert_eq!(format_build_artifact_message(None), "Building artifact");
        assert_eq!(format_build_completed_message(None), "Built");
        assert_eq!(format_prepare_artifact_message(None), "Preparing artifact");
    }

    #[test]
    fn should_use_per_server_spinners_only_for_single_interactive_target() {
        assert!(should_use_per_server_spinners(1, true));
        assert!(!should_use_per_server_spinners(2, true));
        assert!(!should_use_per_server_spinners(1, false));
    }

    #[test]
    fn should_use_local_build_spinners_only_when_interactive() {
        assert!(should_use_local_build_spinners(true));
        assert!(!should_use_local_build_spinners(false));
    }

    #[test]
    fn format_size_uses_expected_units() {
        assert_eq!(format_size(999), "999 bytes");
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1024 * 1024), "1.00 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.00 GB");
    }

    #[test]
    fn format_path_relative_to_returns_project_relative_path_when_possible() {
        let project = Path::new("/repo/examples/javascript/bun");
        let artifact = Path::new("/repo/examples/javascript/bun/.tako/artifacts/a.tar.zst");
        assert_eq!(
            format_path_relative_to(project, artifact),
            ".tako/artifacts/a.tar.zst"
        );
    }

    #[test]
    fn format_path_relative_to_falls_back_to_absolute_when_outside_project() {
        let project = Path::new("/repo/examples/javascript/bun");
        let outside = Path::new("/tmp/a.tar.zst");
        assert_eq!(format_path_relative_to(project, outside), "/tmp/a.tar.zst");
    }

    #[test]
    fn format_bun_lockfile_preflight_error_includes_fix_hint_for_frozen_lockfile_mismatch() {
        let message = format_bun_lockfile_preflight_error(
            "error: lockfile had changes, but lockfile is frozen",
        );
        assert!(message.contains("Bun lockfile check failed"));
        assert!(message.contains("Run `bun install`"));
        assert!(message.contains("bun.lock"));
    }

    #[test]
    fn format_bun_lockfile_preflight_error_falls_back_to_raw_detail() {
        let message = format_bun_lockfile_preflight_error("permission denied");
        assert_eq!(message, "Bun lockfile check failed: permission denied");
    }

    #[test]
    fn format_environment_not_found_error_handles_empty_and_non_empty_env_list() {
        let no_envs = format_environment_not_found_error("production", &[]);
        assert!(no_envs.contains("Environment 'production' not found"));
        assert!(no_envs.contains("(none)"));

        let with_envs = format_environment_not_found_error(
            "staging",
            &["production".to_string(), "dev".to_string()],
        );
        assert!(with_envs.contains("production, dev"));
    }

    #[test]
    fn deploy_error_message_helpers_include_expected_text() {
        let no_servers = format_no_servers_for_env_error("production");
        assert!(no_servers.contains("No servers configured for environment 'production'"));

        let no_global = format_no_global_servers_error();
        assert!(no_global.contains("No servers have been added"));
        assert!(no_global.contains("tako servers add <host>"));

        let missing_server = format_server_not_found_error("prod");
        assert!(missing_server.contains("Server 'prod' not found"));

        let partial = format_partial_failure_error(2);
        assert_eq!(partial, "2 server(s) failed");
    }

    #[test]
    fn build_stage_summary_output_is_hidden_when_empty() {
        let summary: Vec<String> = vec![];
        assert_eq!(format_build_stages_summary_for_output(&summary, None), None);
    }

    #[test]
    fn build_stage_summary_output_is_shown_when_non_empty() {
        let summary = vec!["Stage 'preset'".to_string(), "Stage 2".to_string()];
        assert_eq!(
            format_build_stages_summary_for_output(&summary, Some("linux-x86_64-glibc")),
            Some("Build stages for linux-x86_64-glibc: Stage 'preset' -> Stage 2".to_string())
        );
    }

    #[test]
    fn format_server_targets_summary_deduplicates_target_labels() {
        use crate::config::ServerTarget;
        let summary = format_server_targets_summary(
            &[
                (
                    "a".to_string(),
                    ServerTarget {
                        arch: "x86_64".to_string(),
                        libc: "glibc".to_string(),
                    },
                ),
                (
                    "b".to_string(),
                    ServerTarget {
                        arch: "x86_64".to_string(),
                        libc: "glibc".to_string(),
                    },
                ),
                (
                    "c".to_string(),
                    ServerTarget {
                        arch: "aarch64".to_string(),
                        libc: "musl".to_string(),
                    },
                ),
            ],
            false,
        );

        assert_eq!(
            summary,
            Some("Server targets: linux-aarch64-musl, linux-x86_64-glibc".to_string())
        );
    }

    #[test]
    fn format_server_targets_summary_hides_line_for_unified_mode() {
        use crate::config::ServerTarget;
        let summary = format_server_targets_summary(
            &[(
                "a".to_string(),
                ServerTarget {
                    arch: "aarch64".to_string(),
                    libc: "musl".to_string(),
                },
            )],
            true,
        );

        assert_eq!(summary, None);
    }
}
