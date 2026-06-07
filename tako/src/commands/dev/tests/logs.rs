use super::*;

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
