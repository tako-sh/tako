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
        ..Default::default()
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
        ..Default::default()
    };
    let lines = format_deploy_overview_lines("bun", "production", 1, Some(("testbed", &server)));
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
    let lines = format_deploy_summary_lines_with_https_port(
        "Release",
        "20260330",
        &[
            "app.test".to_string(),
            "app.test/bun".to_string(),
            "*.app.test".to_string(),
        ],
        None,
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
fn deploy_summary_lines_include_non_default_https_port() {
    let lines = format_deploy_summary_lines_with_https_port(
        "Release",
        "20260330",
        &["app.test".to_string(), "app.test/bun".to_string()],
        Some(8443),
    );

    assert_eq!(lines[1].value, "https://app.test:8443");
    assert_eq!(lines[2].value, "https://app.test:8443/bun");
}

#[test]
fn deploy_summary_lines_support_non_url_primary_field() {
    let lines =
        format_deploy_summary_lines_with_https_port("App", "bun", &["app.test".to_string()], None);

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
    let message =
        format_bun_lockfile_preflight_error("error: lockfile had changes, but lockfile is frozen");
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
