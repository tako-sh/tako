use super::client::host_and_port_from_url;
use super::runner::bootstrap_dev_events;
use super::*;
use crate::build::{BuildAdapter, parse_and_validate_preset};
use crate::config::TakoToml;
use crate::dev::LocalCA;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

#[test]
fn resolve_dev_preset_ref_uses_build_adapter_override_when_preset_is_missing() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join("package.json"), r#"{"name":"demo"}"#).unwrap();
    let cfg = TakoToml {
        runtime: Some("node".to_string()),
        ..Default::default()
    };

    assert_eq!(resolve_dev_preset_ref(temp.path(), &cfg).unwrap(), "node");
}

#[test]
fn resolve_dev_preset_ref_qualifies_runtime_local_alias() {
    let temp = TempDir::new().unwrap();
    let cfg = TakoToml {
        runtime: Some("bun".to_string()),
        preset: Some("tanstack-start".to_string()),
        ..Default::default()
    };

    assert_eq!(
        resolve_dev_preset_ref(temp.path(), &cfg).unwrap(),
        "javascript/tanstack-start"
    );
}

#[test]
fn resolve_dev_preset_ref_errors_when_runtime_is_unknown_for_local_alias() {
    let temp = TempDir::new().unwrap();
    let cfg = TakoToml {
        preset: Some("tanstack-start".to_string()),
        ..Default::default()
    };

    let err = resolve_dev_preset_ref(temp.path(), &cfg).unwrap_err();
    assert!(err.contains("Cannot resolve preset"));
}

#[test]
fn resolve_dev_preset_ref_rejects_unknown_build_adapter_override() {
    let temp = TempDir::new().unwrap();
    let cfg = TakoToml {
        runtime: Some("python".to_string()),
        ..Default::default()
    };

    let err = resolve_dev_preset_ref(temp.path(), &cfg).unwrap_err();
    assert!(err.contains("Invalid runtime"));
}

#[test]
fn resolve_effective_dev_build_adapter_uses_preset_group_when_detection_is_unknown() {
    let temp = TempDir::new().unwrap();
    let cfg = TakoToml::default();

    let adapter = resolve_effective_dev_build_adapter(temp.path(), &cfg, "bun").unwrap();
    assert_eq!(adapter, BuildAdapter::Bun);
}

#[test]
fn resolve_dev_run_command_uses_sdk_entrypoint_for_bun() {
    let preset = parse_and_validate_preset(
        r#"
main = "src/index.ts"
"#,
        "bun",
    )
    .unwrap();

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(
        &TakoToml::default(),
        &preset,
        "src/index.ts",
        BuildAdapter::Bun,
        false,
        pd,
    )
    .expect("runtime default dev command");

    assert_eq!(cmd[0], "bun");
    assert!(cmd.iter().any(|a| a.contains("entrypoints/bun-server.mjs")));
    assert!(cmd.last().unwrap().ends_with("src/index.ts"));
}

#[test]
fn resolve_dev_run_command_uses_sdk_entrypoint_for_node() {
    let preset = parse_and_validate_preset(
        r#"
main = "dist/server/tako-entry.mjs"
"#,
        "tanstack-start",
    )
    .unwrap();

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(
        &TakoToml::default(),
        &preset,
        "src/index.ts",
        BuildAdapter::Node,
        true,
        pd,
    )
    .expect("runtime default dev command");

    assert_eq!(cmd[0], "node");
    assert!(
        cmd.iter()
            .any(|a| a.contains("entrypoints/node-server.mjs"))
    );
    assert!(cmd.last().unwrap().ends_with("src/index.ts"));
}

#[test]
fn resolve_dev_run_command_preset_dev_overrides_runtime_default() {
    let mut preset = parse_and_validate_preset(
        r#"
main = "src/index.ts"
"#,
        "vite",
    )
    .unwrap();
    preset.dev = vec!["vite".to_string(), "dev".to_string()];

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(
        &TakoToml::default(),
        &preset,
        "src/index.ts",
        BuildAdapter::Bun,
        true,
        pd,
    )
    .expect("preset dev command");

    assert_eq!(cmd, vec!["vite", "dev"]);
}

#[test]
fn tanstack_start_bun_dev_resolves_to_bunx_bun_vite_dev_end_to_end() {
    let _lock = crate::paths::test_tako_home_env_lock();
    let previous = std::env::var_os("TAKO_HOME");
    let home = TempDir::new().unwrap();
    unsafe {
        std::env::set_var("TAKO_HOME", home.path());
    }

    let project = TempDir::new().unwrap();
    std::fs::write(project.path().join("bun.lock"), "").unwrap();
    std::fs::write(project.path().join("package.json"), r#"{"name":"demo"}"#).unwrap();

    let cfg = TakoToml {
        runtime: Some("bun".to_string()),
        preset: Some("tanstack-start".to_string()),
        ..Default::default()
    };

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let preset_ref = resolve_dev_preset_ref(project.path(), &cfg).unwrap();
    let (preset, _src) = runtime
        .block_on(crate::build::load_dev_build_preset(
            project.path(),
            &preset_ref,
        ))
        .unwrap();

    let adapter = resolve_effective_dev_build_adapter(project.path(), &cfg, &preset_ref).unwrap();

    let cmd = resolve_dev_run_command(&cfg, &preset, "src/index.ts", adapter, true, project.path())
        .unwrap();

    match previous {
        Some(value) => unsafe { std::env::set_var("TAKO_HOME", value) },
        None => unsafe { std::env::remove_var("TAKO_HOME") },
    }

    assert_eq!(adapter, BuildAdapter::Bun);
    assert_eq!(preset_ref, "javascript/tanstack-start");
    assert_eq!(cmd, vec!["bun", "--bun", "./node_modules/.bin/vite", "dev"]);
}

#[test]
fn resolve_dev_run_command_uses_preset_runtime_override_for_bun() {
    let preset = parse_and_validate_preset(
        r#"
main = "src/index.ts"
dev = ["vite", "dev"]

[bun]
dev = ["bunx", "--bun", "vite", "dev"]
"#,
        "tanstack-start",
    )
    .unwrap();

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(
        &TakoToml::default(),
        &preset,
        "src/index.ts",
        BuildAdapter::Bun,
        true,
        pd,
    )
    .expect("preset runtime override command");

    assert_eq!(cmd, vec!["bunx", "--bun", "vite", "dev"]);
}

#[test]
fn resolve_dev_run_command_falls_back_to_preset_dev_when_runtime_override_missing() {
    let preset = parse_and_validate_preset(
        r#"
main = "src/index.ts"
dev = ["vite", "dev"]

[bun]
dev = ["bunx", "--bun", "vite", "dev"]
"#,
        "tanstack-start",
    )
    .unwrap();

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(
        &TakoToml::default(),
        &preset,
        "src/index.ts",
        BuildAdapter::Node,
        true,
        pd,
    )
    .expect("preset default dev command for node");

    assert_eq!(cmd, vec!["vite", "dev"]);
}

#[test]
fn resolve_dev_run_command_config_dev_beats_runtime_override() {
    let preset = parse_and_validate_preset(
        r#"
main = "src/index.ts"
dev = ["vite", "dev"]

[bun]
dev = ["bunx", "--bun", "vite", "dev"]
"#,
        "tanstack-start",
    )
    .unwrap();

    let cfg = TakoToml {
        dev: vec!["custom".to_string(), "cmd".to_string()],
        ..Default::default()
    };

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(&cfg, &preset, "src/index.ts", BuildAdapter::Bun, true, pd)
        .expect("config dev command");

    assert_eq!(cmd, vec!["custom", "cmd"]);
}

#[test]
fn resolve_dev_run_command_config_dev_overrides_preset() {
    let mut preset = parse_and_validate_preset(
        r#"
main = "src/index.ts"
"#,
        "vite",
    )
    .unwrap();
    preset.dev = vec!["vite".to_string(), "dev".to_string()];

    let cfg = TakoToml {
        dev: vec!["custom".to_string(), "cmd".to_string()],
        ..Default::default()
    };

    let pd = Path::new("/project");
    let cmd = resolve_dev_run_command(&cfg, &preset, "src/index.ts", BuildAdapter::Bun, true, pd)
        .expect("config dev command");

    assert_eq!(cmd, vec!["custom", "cmd"]);
}

#[test]
fn readiness_failure_hint_for_dev_command_detects_vite_commands() {
    for cmd in [
        vec!["vite".to_string()],
        vec!["vite".to_string(), "dev".to_string()],
        vec![
            "bun".to_string(),
            "--bun".to_string(),
            "./node_modules/.bin/vite".to_string(),
            "dev".to_string(),
        ],
    ] {
        let hint = readiness_failure_hint_for_dev_command(&cmd).unwrap();
        assert!(hint.contains("tako.sh/vite"));
    }
}

#[test]
fn readiness_failure_hint_for_dev_command_ignores_package_scripts() {
    let cmd = vec!["bun".to_string(), "run".to_string(), "dev".to_string()];

    assert!(readiness_failure_hint_for_dev_command(&cmd).is_none());
}

#[test]
fn resolve_dev_worker_command_returns_none_without_workflows_dir() {
    let temp = TempDir::new().unwrap();
    let cmd = resolve_dev_worker_command(temp.path(), "src", BuildAdapter::Bun);
    assert!(cmd.is_none());
}

#[test]
fn resolve_dev_worker_command_returns_none_for_non_js_runtime() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("src").join("workflows")).unwrap();
    assert!(resolve_dev_worker_command(temp.path(), "src", BuildAdapter::Go).is_none());
    assert!(resolve_dev_worker_command(temp.path(), "src", BuildAdapter::Unknown).is_none());
}

#[test]
fn resolve_dev_worker_command_bun_points_at_sdk_worker_entrypoint() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("src").join("workflows")).unwrap();
    let cmd = resolve_dev_worker_command(temp.path(), "src", BuildAdapter::Bun).unwrap();
    assert_eq!(cmd[0], "bun");
    assert!(cmd.iter().any(|a| a.contains("entrypoints/bun-worker.mjs")));
    assert!(!cmd.iter().any(|a| a.contains("{main}")));
}

#[test]
fn resolve_dev_worker_command_node_uses_strip_types_and_worker_entrypoint() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("src").join("workflows")).unwrap();
    let cmd = resolve_dev_worker_command(temp.path(), "src", BuildAdapter::Node).unwrap();
    assert_eq!(cmd[0], "node");
    assert!(cmd.iter().any(|a| a == "--experimental-strip-types"));
    assert!(
        cmd.iter()
            .any(|a| a.contains("entrypoints/node-worker.mjs"))
    );
}

#[test]
fn resolve_dev_worker_command_uses_configured_app_root() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("app").join("workflows")).unwrap();

    assert!(resolve_dev_worker_command(temp.path(), "app", BuildAdapter::Bun).is_some());
    assert!(resolve_dev_worker_command(temp.path(), "src", BuildAdapter::Bun).is_none());
}

#[test]
fn dev_startup_lines_quiet_is_short() {
    let lines = dev_startup_lines(
        false,
        "app",
        "fake",
        Path::new("index.ts"),
        "https://app.test:8443/",
    );
    assert_eq!(lines[0], "https://app.test:8443/");
    assert!(lines.iter().all(|l| !l.contains("Tako Dev Server")));
}

#[test]
fn dev_startup_lines_verbose_includes_banner() {
    let lines = dev_startup_lines(
        true,
        "app",
        "fake",
        Path::new("index.ts"),
        "https://app.test:8443/",
    );
    assert!(lines.iter().any(|l| l == "Tako Dev Server"));
    assert!(lines.iter().any(|l| l.starts_with("URL:")));
}

#[test]
fn inject_dev_data_dir_creates_nested_app_and_tako_dirs() {
    let temp = TempDir::new().unwrap();
    let mut env = std::collections::HashMap::new();

    inject_dev_data_dir(temp.path(), &mut env).unwrap();

    assert_eq!(
        env.get("TAKO_DATA_DIR").map(String::as_str),
        Some(
            temp.path()
                .join(".tako/data/app")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(dev_runtime_data_root(temp.path()).join("app").is_dir());
    assert!(dev_runtime_data_root(temp.path()).join("tako").is_dir());
}

#[test]
fn compute_dev_env_ignores_configured_env_and_derives_development() {
    let cfg = TakoToml::parse(
        r#"
[vars]
ENV = "custom"
"#,
    )
    .unwrap();

    let env = compute_dev_env(&cfg);
    assert_eq!(env.get("ENV").map(String::as_str), Some("development"));
}

#[test]
fn compute_dev_env_passes_through_user_log_level_from_vars() {
    let cfg = TakoToml::parse(
        r#"
[vars.development]
LOG_LEVEL = "debug"
"#,
    )
    .unwrap();

    let env = compute_dev_env(&cfg);
    assert_eq!(env.get("LOG_LEVEL").map(String::as_str), Some("debug"));
}

#[test]
fn inject_dev_allowed_hosts_exports_route_hosts_for_vite() {
    let mut env = std::collections::HashMap::new();
    let hosts = vec![
        "app.test".to_string(),
        "tunnel.example.com".to_string(),
        "tunnel.example.com/api".to_string(),
        "*.preview.example.com".to_string(),
    ];

    inject_dev_allowed_hosts(&hosts, &mut env);

    assert_eq!(
        env.get("TAKO_DEV_ALLOWED_HOSTS").map(String::as_str),
        Some("app.test,tunnel.example.com,.preview.example.com")
    );
}

#[tokio::test]
async fn tcp_probe_detects_open_port() {
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
        return;
    };
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = listener.accept().await;
    });

    assert!(tcp_probe(("127.0.0.1", port), 200).await);
}

#[tokio::test]
async fn tcp_probe_detects_closed_port() {
    assert!(!tcp_probe(("127.0.0.1", 0), 50).await);
}

#[test]
fn bootstrap_dev_events_marks_running_app_ready_when_pid_is_known() {
    let events = bootstrap_dev_events("running", Some(4242));

    assert_eq!(events.len(), 2);
    match &events[0] {
        DevEvent::AppPid(pid) => assert_eq!(pid, &4242),
        other => panic!("expected AppPid, got {other:?}"),
    }
    assert!(matches!(events[1], DevEvent::AppReady));
}

#[test]
fn bootstrap_dev_events_marks_idle_app_stopped() {
    let events = bootstrap_dev_events("idle", None);

    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], DevEvent::AppStopped));
}

#[test]
fn bootstrap_dev_events_waits_for_pid_before_marking_running() {
    let events = bootstrap_dev_events("running", None);

    assert!(events.is_empty());
}

#[tokio::test]
async fn tcp_probe_retries_until_port_is_open() {
    let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await else {
        return;
    };
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await else {
            return;
        };
        let _ = listener.accept().await;
    });

    let mut ok = false;
    for _ in 0..10 {
        if tcp_probe(("127.0.0.1", port), 10).await {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(ok);
}

#[tokio::test]
async fn tcp_probe_returns_false_for_closed_port() {
    assert!(!tcp_probe(("127.0.0.1", 0), 10).await);
}

#[tokio::test]
async fn wait_for_dev_server_stopped_waits_for_socket_path_to_disappear() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("dev-server.sock");
    std::fs::write(&socket_path, "stale socket path").unwrap();

    let remove_path = socket_path.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = tokio::fs::remove_file(remove_path).await;
    });

    let start = std::time::Instant::now();
    prepare::wait_for_dev_server_stopped_with_socket_path("127.0.0.1:59091", Some(&socket_path))
        .await;

    assert!(
        start.elapsed() >= Duration::from_millis(150),
        "returned before socket path cleanup completed"
    );
}

#[test]
fn log_level_display_uses_five_levels() {
    assert_eq!(LogLevel::Debug.to_string(), "DEBUG");
    assert_eq!(LogLevel::Info.to_string(), "INFO");
    assert_eq!(LogLevel::Warn.to_string(), "WARN");
    assert_eq!(LogLevel::Error.to_string(), "ERROR");
    assert_eq!(LogLevel::Fatal.to_string(), "FATAL");
}

#[test]
fn dev_starts_with_one_instance() {
    assert_eq!(dev_initial_instance_count(), 1);
}

#[test]
fn dev_idle_timeout_is_thirty_minutes() {
    assert_eq!(dev_idle_timeout(), Duration::from_secs(30 * 60));
}

#[test]
fn app_logs_use_app_scope() {
    assert_eq!(app_log_scope(), "app");
}

#[test]
fn child_log_level_parser_extracts_debug_and_message() {
    let (level, message) = child_log_level_and_message(LogLevel::Info, "[DEBUG] hello");
    assert!(matches!(level, LogLevel::Debug));
    assert_eq!(message, "hello");
}

#[test]
fn child_log_level_parser_extracts_warning_prefixes_case_insensitively() {
    let (level, message) = child_log_level_and_message(LogLevel::Info, "warning: low disk");
    assert!(matches!(level, LogLevel::Warn));
    assert_eq!(message, "low disk");
}

#[test]
fn child_log_level_parser_maps_trace_to_debug() {
    let (level, message) = child_log_level_and_message(LogLevel::Info, "trace startup");
    assert!(matches!(level, LogLevel::Debug));
    assert_eq!(message, "startup");
}

#[test]
fn child_log_level_parser_falls_back_when_no_level_prefix_exists() {
    let (level, message) = child_log_level_and_message(LogLevel::Warn, "connected");
    assert!(matches!(level, LogLevel::Warn));
    assert_eq!(message, "connected");
}

#[test]
fn child_log_level_parser_does_not_match_partial_prefixes() {
    let (level, message) = child_log_level_and_message(LogLevel::Info, "debugger attached");
    assert!(matches!(level, LogLevel::Info));
    assert_eq!(message, "debugger attached");
}

#[test]
fn child_log_filter_drops_shell_command_echo_lines() {
    assert!(should_drop_child_log_line("$ vite dev"));
    assert!(should_drop_child_log_line("  $ bun run dev  "));
    assert!(should_drop_child_log_line(""));
}

#[test]
fn child_log_filter_keeps_non_command_messages() {
    assert!(!should_drop_child_log_line("warning: low disk"));
    assert!(!should_drop_child_log_line("$5 price update"));
    assert!(!should_drop_child_log_line("$$$"));
}

#[test]
fn child_log_message_trim_keeps_leading_alignment_and_removes_trailing_whitespace() {
    assert_eq!(
        trim_child_log_message("  VITE v7 ready   "),
        Some("  VITE v7 ready".to_string())
    );
}

#[test]
fn child_log_message_trim_drops_whitespace_only_lines() {
    assert_eq!(trim_child_log_message("   "), None);
}

#[test]
fn parse_log_line_accepts_sdk_schema_with_lowercase_level() {
    let line = r#"{"ts":1700000000000,"level":"info","scope":"vite","msg":"hello"}"#;
    let decoded = parse_log_line(line).unwrap();

    assert_eq!(decoded.scope, "vite");
    assert_eq!(decoded.message, "hello");
    assert!(matches!(decoded.level, LogLevel::Info));
}

#[test]
fn parse_log_line_accepts_all_levels_lowercase() {
    for (level_str, expected) in [
        ("debug", LogLevel::Debug),
        ("info", LogLevel::Info),
        ("warn", LogLevel::Warn),
        ("error", LogLevel::Error),
    ] {
        let line = format!(
            r#"{{"ts":1700000000000,"level":"{}","scope":"x","msg":"y"}}"#,
            level_str
        );
        let decoded = parse_log_line(&line).unwrap();
        assert!(
            std::mem::discriminant(&decoded.level) == std::mem::discriminant(&expected),
            "level {} did not parse",
            level_str
        );
    }
}

#[test]
fn parse_log_line_converts_ts_millis_to_hms() {
    let line = r#"{"ts":1700000000000,"level":"info","scope":"x","msg":"y"}"#;
    let decoded = parse_log_line(line).unwrap();
    // Format is "HH:MM:SS"
    assert_eq!(decoded.timestamp.len(), 8);
    assert_eq!(decoded.timestamp.chars().nth(2), Some(':'));
    assert_eq!(decoded.timestamp.chars().nth(5), Some(':'));
}

#[test]
fn parse_log_line_captures_fields_payload() {
    let line = r#"{"ts":1700000000000,"level":"info","scope":"vite","msg":"bound","fields":{"port":5173,"build":"abc"}}"#;
    let decoded = parse_log_line(line).unwrap();
    let fields = decoded.fields.as_ref().expect("fields should be present");
    assert_eq!(fields.get("port").and_then(|v| v.as_u64()), Some(5173));
    assert_eq!(fields.get("build").and_then(|v| v.as_str()), Some("abc"));
}

#[test]
fn parse_log_line_accepts_log_with_no_fields_key() {
    let line = r#"{"ts":1700000000000,"level":"info","scope":"vite","msg":"hi"}"#;
    let decoded = parse_log_line(line).unwrap();
    assert!(decoded.fields.is_none());
}

#[test]
fn parse_log_line_falls_back_to_app_scope_for_plain_text() {
    let decoded = parse_log_line("not json").unwrap();
    assert_eq!(decoded.scope, "app");
    assert_eq!(decoded.message, "not json");
    assert!(matches!(decoded.level, LogLevel::Info));
}

#[test]
fn parse_log_line_falls_back_for_malformed_json_starting_with_brace() {
    let decoded = parse_log_line("{not valid json").unwrap();
    assert_eq!(decoded.scope, "app");
    assert_eq!(decoded.message, "{not valid json");
}

#[test]
fn restart_not_required_when_no_existing_server() {
    assert!(!restart_required_for_requested_listen(
        None,
        "127.0.0.1:47831"
    ));
}

#[test]
fn restart_not_required_when_existing_listen_matches() {
    assert!(!restart_required_for_requested_listen(
        Some("127.0.0.1:47831"),
        "127.0.0.1:47831"
    ));
}

#[test]
fn restart_required_when_existing_listen_differs() {
    assert!(restart_required_for_requested_listen(
        Some("127.0.0.1:8443"),
        "127.0.0.1:47831"
    ));
}

#[test]
fn parse_port_from_listen_handles_valid_and_invalid_values() {
    assert_eq!(port_from_listen("127.0.0.1:47831"), Some(47831));
    assert_eq!(port_from_listen("localhost:443"), Some(443));
    assert_eq!(port_from_listen("bad-listen"), None);
    assert_eq!(port_from_listen("host:not-a-port"), None);
}

#[test]
fn host_and_port_parser_handles_default_and_explicit_ports() {
    assert_eq!(
        host_and_port_from_url("https://app.test/"),
        Some(("app.test".to_string(), 443))
    );
    assert_eq!(
        host_and_port_from_url("https://app.test:47831/"),
        Some(("app.test".to_string(), 47831))
    );
}

#[test]
fn doctor_omits_duplicate_port_line_when_listen_includes_same_port() {
    let lines = doctor_dev_server_lines("127.0.0.1:47831", 47831, false, false, true, 53535);
    assert!(
        !lines.iter().any(|line| line.starts_with("  port:")),
        "doctor output should not duplicate listen port: {lines:?}"
    );
}

#[test]
fn doctor_keeps_port_line_when_listen_does_not_include_port() {
    let lines = doctor_dev_server_lines("(unknown)", 47831, false, false, true, 53535);
    assert!(
        lines.iter().any(|line| line == "  port: 47831"),
        "doctor output should keep explicit port when listen does not include one: {lines:?}"
    );
}

#[test]
fn doctor_preflight_lines_show_proxy_not_loaded() {
    let lines = doctor_local_forwarding_preflight_lines("127.77.0.1", false, false, true);
    assert!(lines.iter().any(|line| line.contains("not loaded")));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("TCP 127.77.0.1:443 (unreachable)"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("TCP 127.77.0.1:80 (ok)"))
    );
}

#[test]
fn doctor_preflight_lines_show_proxy_loaded() {
    let lines = doctor_local_forwarding_preflight_lines("127.77.0.1", true, true, true);
    assert!(lines.iter().any(|line| line.contains("loaded")));
}

#[test]
fn unavailable_error_detection_matches_missing_or_stale_socket_errors() {
    assert!(is_dev_server_unavailable_error_message(
        "No such file or directory (os error 2)"
    ));
    assert!(is_dev_server_unavailable_error_message(
        "Connection refused (os error 61)"
    ));
    assert!(is_dev_server_unavailable_error_message(
        "Operation not permitted (os error 1)"
    ));
    assert!(is_dev_server_unavailable_error_message(
        "Permission denied (os error 13)"
    ));
    assert!(!is_dev_server_unavailable_error_message(
        "failed to parse response"
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn local_dns_resolver_template_targets_loopback_port() {
    assert_eq!(
        local_dns_resolver_contents(53535),
        "nameserver 127.0.0.1\nport 53535\n"
    );
}

#[test]
fn dev_server_tls_paths_are_under_certs_dir() {
    let home = Path::new("/tmp/tako-home");
    let (cert_path, key_path) = dev_server_tls_paths_for_home(home);
    assert_eq!(
        cert_path,
        Path::new("/tmp/tako-home/certs/fullchain.pem").to_path_buf()
    );
    assert_eq!(
        key_path,
        Path::new("/tmp/tako-home/certs/privkey.pem").to_path_buf()
    );
}

#[test]
fn ensure_dev_server_tls_material_writes_cert_and_key_when_missing() {
    let temp = TempDir::new().unwrap();
    let ca = LocalCA::generate().unwrap();
    let changed = ensure_dev_server_tls_material_for_home(&ca, temp.path(), "demo").unwrap();
    assert!(changed);

    let (cert_path, key_path) = dev_server_tls_paths_for_home(temp.path());
    let names_path = dev_server_tls_names_path_for_home(temp.path());
    let cert = std::fs::read_to_string(cert_path).unwrap();
    let key = std::fs::read_to_string(key_path).unwrap();
    let names = std::fs::read_to_string(names_path).unwrap();
    assert!(cert.contains("BEGIN CERTIFICATE"));
    assert!(key.contains("BEGIN PRIVATE KEY"));
    assert!(names.contains("*.demo.test"));
}

#[test]
fn ensure_dev_server_tls_material_keeps_existing_files() {
    let temp = TempDir::new().unwrap();
    let (cert_path, key_path) = dev_server_tls_paths_for_home(temp.path());
    let names_path = dev_server_tls_names_path_for_home(temp.path());
    std::fs::create_dir_all(cert_path.parent().unwrap()).unwrap();
    std::fs::write(&cert_path, "existing-cert").unwrap();
    std::fs::write(&key_path, "existing-key").unwrap();
    std::fs::write(
        &names_path,
        r#"[
  "*.demo.tako.test",
  "*.demo.test",
  "*.tako.test",
  "*.test",
  "demo.tako.test",
  "demo.test",
  "tako.test",
  "test"
]"#,
    )
    .unwrap();

    let ca = LocalCA::generate().unwrap();
    // Write matching CA fingerprint so the check passes.
    std::fs::write(
        ca_fingerprint_path_for_home(temp.path()),
        ca_fingerprint(&ca),
    )
    .unwrap();

    let changed = ensure_dev_server_tls_material_for_home(&ca, temp.path(), "demo").unwrap();
    assert!(!changed);

    let cert = std::fs::read_to_string(cert_path).unwrap();
    let key = std::fs::read_to_string(key_path).unwrap();
    assert_eq!(cert, "existing-cert");
    assert_eq!(key, "existing-key");
}

#[test]
fn ensure_dev_server_tls_material_regenerates_when_ca_changes() {
    let temp = TempDir::new().unwrap();
    let ca1 = LocalCA::generate().unwrap();
    let changed = ensure_dev_server_tls_material_for_home(&ca1, temp.path(), "demo").unwrap();
    assert!(changed);

    // Same CA, same names → no change.
    let changed = ensure_dev_server_tls_material_for_home(&ca1, temp.path(), "demo").unwrap();
    assert!(!changed);

    // Different CA, same names → must regenerate.
    let ca2 = LocalCA::generate().unwrap();
    let changed = ensure_dev_server_tls_material_for_home(&ca2, temp.path(), "demo").unwrap();
    assert!(changed);
}

#[test]
fn ensure_dev_server_tls_material_regenerates_files_without_names_manifest() {
    let temp = TempDir::new().unwrap();
    let (cert_path, key_path) = dev_server_tls_paths_for_home(temp.path());
    std::fs::create_dir_all(cert_path.parent().unwrap()).unwrap();
    std::fs::write(&cert_path, "existing-cert").unwrap();
    std::fs::write(&key_path, "existing-key").unwrap();

    let ca = LocalCA::generate().unwrap();
    let changed = ensure_dev_server_tls_material_for_home(&ca, temp.path(), "demo").unwrap();
    assert!(changed);

    let cert = std::fs::read_to_string(&cert_path).unwrap();
    let key = std::fs::read_to_string(&key_path).unwrap();
    let names = std::fs::read_to_string(dev_server_tls_names_path_for_home(temp.path())).unwrap();
    assert!(cert.contains("BEGIN CERTIFICATE"));
    assert!(key.contains("BEGIN PRIVATE KEY"));
    assert!(names.contains("*.demo.test"));
}

#[test]
fn ensure_dev_server_tls_material_merges_names_for_multiple_apps() {
    let temp = TempDir::new().unwrap();
    let ca = LocalCA::generate().unwrap();
    let first_changed = ensure_dev_server_tls_material_for_home(&ca, temp.path(), "alpha")
        .expect("first cert write");
    assert!(first_changed);
    let second_changed = ensure_dev_server_tls_material_for_home(&ca, temp.path(), "beta")
        .expect("second cert write");
    assert!(second_changed);

    let names = std::fs::read_to_string(dev_server_tls_names_path_for_home(temp.path())).unwrap();
    assert!(names.contains("*.alpha.test"));
    assert!(names.contains("*.beta.test"));
}

#[cfg(target_os = "macos")]
#[test]
fn parse_local_dns_resolver_extracts_nameserver_and_port() {
    let (ns, port) =
        parse_local_dns_resolver("# tako resolver\nnameserver 127.0.0.1\nport 53535\n");
    assert_eq!(ns.as_deref(), Some("127.0.0.1"));
    assert_eq!(port, Some(53535));
}

#[cfg(target_os = "macos")]
#[test]
fn parse_local_dns_resolver_prefers_latest_valid_entries() {
    let (ns, port) = parse_local_dns_resolver(
        "# stale resolver values\nnameserver 10.0.0.1\nport not-a-number\nnameserver 127.0.0.1\nport 53535\n",
    );
    assert_eq!(ns.as_deref(), Some("127.0.0.1"));
    assert_eq!(port, Some(53535));
}

#[cfg(target_os = "macos")]
#[test]
fn parse_local_dns_resolver_ignores_unknown_lines() {
    let (ns, port) = parse_local_dns_resolver(
        "# unrelated\nsearch local\noptions ndots:1\nnameserver 127.0.0.1\n",
    );
    assert_eq!(ns.as_deref(), Some("127.0.0.1"));
    assert_eq!(port, None);
}

#[cfg(target_os = "macos")]
#[test]
fn ensure_local_dns_resolver_non_interactive_error_is_actionable() {
    let err = ensure_local_dns_resolver_configured(65535)
        .expect_err("non-interactive setup should fail when resolver is not configured");
    let text = err.to_string();
    assert!(text.contains("/etc/resolver/tako"));
    assert!(text.contains("run `tako dev` interactively once"));
}

#[cfg(target_os = "macos")]
#[test]
fn sudo_setup_action_items_uses_expected_order() {
    let items = sudo_setup_action_items(
        Some("Trust the Tako local CA for trusted https://*.test"),
        true,
        Some("Install the local dev proxy for 127.77.0.1:80/443"),
    );
    assert_eq!(
        items,
        vec![
            "Trust the Tako local CA for trusted https://*.test".to_string(),
            local_dns_sudo_action_line().to_string(),
            "Install the local dev proxy for 127.77.0.1:80/443".to_string(),
        ]
    );
}

#[cfg(target_os = "macos")]
#[test]
fn sudo_setup_action_items_omits_absent_steps() {
    let items = sudo_setup_action_items(None, false, Some("Repair dev proxy"));
    assert_eq!(items, vec!["Repair dev proxy".to_string()]);
}

#[test]
fn prefers_local_url_when_80_443_forwarding_is_detected() {
    let url = preferred_public_url(
        "bun-example.test",
        "https://bun-example.test:47831/",
        47831,
        443,
    );
    assert_eq!(url, "https://bun-example.test/");
}

#[test]
fn prefers_daemon_url_when_display_and_listen_ports_match() {
    let url = preferred_public_url(
        "bun-example.test",
        "https://bun-example.test:47831/",
        47831,
        47831,
    );
    assert_eq!(url, "https://bun-example.test:47831/");
}

#[test]
fn display_routes_always_includes_default() {
    let cfg = TakoToml::default();
    let routes = compute_display_routes(&cfg, "app.test", None);
    assert_eq!(routes, vec!["app.test"]);
}

#[test]
fn display_routes_omit_default_when_explicit_routes_configured() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"app.test/bun\", \"*.app.test\"]\n")
        .unwrap();
    let routes = compute_display_routes(&cfg, "app.test", None);
    assert_eq!(routes, vec!["app.test/bun", "*.app.test"]);
}

#[test]
fn display_routes_use_user_configured_default_as_sole_route() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"app.test\"]\n").unwrap();
    let routes = compute_display_routes(&cfg, "app.test", None);
    assert_eq!(routes, vec!["app.test"]);
}

#[test]
fn display_routes_rewrite_wildcard_for_variant() {
    let cfg = TakoToml::parse(
        "[envs.development]\nroutes = [\"some-app.test/bun\", \"*.example.test\"]\n",
    )
    .unwrap();
    let routes = compute_display_routes(&cfg, "example-foo.test", Some("example.test"));
    assert_eq!(routes, vec!["some-app.test/bun", "*.example-foo.test",]);
}

#[test]
fn display_routes_variant_rewrites_base_domain_in_user_routes() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"example.test\"]\n").unwrap();
    let routes = compute_display_routes(&cfg, "example-foo.test", Some("example.test"));
    assert_eq!(routes, vec!["example-foo.test"]);
}

#[test]
fn display_routes_include_default_for_external_only_routes() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"tunnel.example.com\"]\n").unwrap();
    let routes = compute_display_routes(&cfg, "app.test", None);
    assert_eq!(routes, vec!["app.test", "tunnel.example.com"]);
}

#[test]
fn local_https_probe_host_uses_app_test_domain() {
    assert_eq!(
        local_https_probe_host("bun-example.test"),
        "bun-example.test"
    );
}

#[test]
fn falls_back_to_default_host_when_development_routes_are_missing() {
    let cfg = TakoToml::default();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["app.test".to_string()]);
}

#[test]
fn falls_back_to_default_host_when_development_routes_are_empty() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = []\n").unwrap();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["app.test".to_string()]);
}

#[test]
fn explicit_routes_omit_default_host() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"api.app.test\"]\n").unwrap();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["api.app.test"]);
}

#[test]
fn external_only_routes_keep_default_host() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"tunnel.example.com\"]\n").unwrap();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["app.test", "tunnel.example.com"]);
}

#[test]
fn external_routes_are_additive_to_explicit_dev_routes() {
    let cfg = TakoToml::parse(
        "[envs.development]\nroutes = [\"api.app.test\", \"tunnel.example.com\"]\n",
    )
    .unwrap();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["api.app.test", "tunnel.example.com"]);
}

#[test]
fn wildcard_only_routes_omit_default_host() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"*.app.test\"]\n").unwrap();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["*.app.test"]);
}

#[test]
fn user_default_host_as_sole_route_passes_through() {
    let cfg = TakoToml::parse("[envs.development]\nroutes = [\"app.test\"]\n").unwrap();
    let hosts = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();
    assert_eq!(hosts, vec!["app.test"]);
}

#[test]
fn dev_hosts_rewrite_wildcard_for_variant() {
    let cfg = TakoToml::parse(
        "[envs.development]\nroutes = [\"some-app.test/bun\", \"*.example.test\"]\n",
    )
    .unwrap();
    let hosts = compute_dev_hosts(
        "example-foo",
        &cfg,
        "example-foo.test",
        Some("example.test"),
    )
    .unwrap();
    assert_eq!(hosts, vec!["some-app.test/bun", "*.example-foo.test",]);
}

#[test]
fn dev_hosts_now_include_paths_and_wildcards() {
    let cfg = TakoToml::parse(
        "[envs.development]\nroutes = [\"app.test\", \"app.test/api\", \"*.app.test\"]\n",
    )
    .unwrap();
    let display = compute_display_routes(&cfg, "app.test", None);
    let routing = compute_dev_hosts("app", &cfg, "app.test", None).unwrap();

    assert_eq!(display, vec!["app.test", "app.test/api", "*.app.test"]);
    assert_eq!(routing, vec!["app.test", "app.test/api", "*.app.test"]);
}

#[test]
fn route_hostname_matches_exact() {
    assert!(route_hostname_matches("app.test", "app.test"));
    assert!(!route_hostname_matches("app.test", "other.test"));
}

#[test]
fn route_hostname_matches_with_path() {
    assert!(route_hostname_matches("app.test/api", "app.test"));
    assert!(!route_hostname_matches("app.test/api", "other.test"));
}

#[test]
fn route_hostname_matches_wildcard() {
    assert!(route_hostname_matches("*.app.test", "foo.app.test"));
    assert!(!route_hostname_matches("*.app.test", "app.test"));
    assert!(!route_hostname_matches("*.app.test", "other.test"));
}

#[test]
fn sanitize_name_segment_lowercases() {
    assert_eq!(sanitize_name_segment("MyApp"), "myapp");
}

#[test]
fn sanitize_name_segment_replaces_special_chars() {
    assert_eq!(sanitize_name_segment("foo_bar.baz"), "foo-bar-baz");
}

#[test]
fn sanitize_name_segment_collapses_consecutive_separators() {
    assert_eq!(sanitize_name_segment("a__b--c..d"), "a-b-c-d");
}

#[test]
fn sanitize_name_segment_strips_leading_trailing_hyphens() {
    assert_eq!(sanitize_name_segment("-abc-"), "abc");
}

#[test]
fn sanitize_name_segment_drops_non_ascii() {
    assert_eq!(sanitize_name_segment("café"), "caf");
}

#[test]
fn short_path_hash_is_deterministic() {
    let a = short_path_hash("/home/user/project");
    let b = short_path_hash("/home/user/project");
    assert_eq!(a, b);
}

#[test]
fn short_path_hash_differs_for_different_paths() {
    let a = short_path_hash("/home/user/project-a");
    let b = short_path_hash("/home/user/project-b");
    assert_ne!(a, b);
}

#[test]
fn short_path_hash_is_4_hex_chars() {
    let h = short_path_hash("/some/path");
    assert_eq!(h.len(), 4);
    assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn no_existing_apps_returns_candidate_unchanged() {
    let result = disambiguate_app_name("my-app", "/proj", &[]);
    assert_eq!(result, "my-app");
}

#[test]
fn same_project_dir_is_not_a_conflict() {
    let existing = vec![("my-app".into(), "/proj/tako.toml".into())];
    let result = disambiguate_app_name("my-app", "/proj/tako.toml", &existing);
    assert_eq!(result, "my-app");
}

#[test]
fn different_name_is_not_a_conflict() {
    let existing = vec![("other-app".into(), "/other/tako.toml".into())];
    let result = disambiguate_app_name("my-app", "/proj/tako.toml", &existing);
    assert_eq!(result, "my-app");
}

#[test]
fn conflict_appends_dir_leaf_name() {
    let existing = vec![("my-app".into(), "/home/user/proj-a/tako.toml".into())];
    let result = disambiguate_app_name("my-app", "/home/user/proj-b/tako.toml", &existing);
    assert_eq!(result, "my-app-proj-b");
}

#[test]
fn conflict_from_variant_matching_existing_app_name() {
    let existing = vec![("app-foo".into(), "/proj/app-foo/tako.toml".into())];
    let result = disambiguate_app_name("app-foo", "/proj/app/tako.toml", &existing);
    assert_eq!(result, "app-foo-app");
}

#[test]
fn conflict_from_non_variant_matching_variant_composite() {
    let existing = vec![("app-foo".into(), "/proj/app/tako.toml".into())];
    let result = disambiguate_app_name("app-foo", "/proj/app-foo/tako.toml", &existing);
    assert_eq!(result, "app-foo-app-foo");
}

#[test]
fn double_conflict_falls_back_to_hash() {
    let existing = vec![
        ("my-app".into(), "/workspace/a/tako.toml".into()),
        ("my-app-b".into(), "/workspace/b/tako.toml".into()),
    ];
    let result = disambiguate_app_name("my-app", "/workspace/c/b/tako.toml", &existing);
    let hash = short_path_hash("/workspace/c/b/tako.toml");
    assert_eq!(result, format!("my-app-{hash}"));
}

#[test]
fn workspace_apps_get_folder_suffix() {
    let existing = vec![("api".into(), "/repo/packages/billing/tako.toml".into())];
    let result = disambiguate_app_name("api", "/repo/packages/payments/tako.toml", &existing);
    assert_eq!(result, "api-payments");
}

#[test]
fn two_checkouts_of_same_repo_get_folder_suffix() {
    let existing = vec![("my-app".into(), "/home/user/my-app-main/tako.toml".into())];
    let result = disambiguate_app_name("my-app", "/home/user/my-app-feature/tako.toml", &existing);
    assert_eq!(result, "my-app-my-app-feature");
}

#[test]
fn no_conflict_among_many_registered_apps() {
    let existing = vec![
        ("alpha".into(), "/a/tako.toml".into()),
        ("beta".into(), "/b/tako.toml".into()),
        ("gamma".into(), "/c/tako.toml".into()),
    ];
    let result = disambiguate_app_name("delta", "/d/tako.toml", &existing);
    assert_eq!(result, "delta");
}

#[test]
fn conflict_detected_among_many_registered_apps() {
    let existing = vec![
        ("alpha".into(), "/a/tako.toml".into()),
        ("beta".into(), "/b/tako.toml".into()),
        ("gamma".into(), "/c/tako.toml".into()),
    ];
    let result = disambiguate_app_name("beta", "/other/tako.toml", &existing);
    assert_eq!(result, "beta-other");
}

#[test]
fn root_path_project_uses_hash_fallback() {
    let existing = vec![("app".into(), "/other/tako.toml".into())];
    let result = disambiguate_app_name("app", "/tako.toml", &existing);
    let hash = short_path_hash("/tako.toml");
    assert_eq!(result, format!("app-{hash}"));
}

#[test]
fn re_registration_after_disambiguation_is_idempotent() {
    let existing = vec![
        ("api".into(), "/repo/packages/billing/tako.toml".into()),
        (
            "api-payments".into(),
            "/repo/packages/payments/tako.toml".into(),
        ),
    ];
    let result = disambiguate_app_name("api", "/repo/packages/payments/tako.toml", &existing);
    assert_eq!(result, "api-payments");
}
