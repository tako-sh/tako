use super::*;

#[test]
fn format_log_fields() {
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "app".to_string(),
        message: "hello".to_string(),
        fields: None,
        kind: None,
    };
    let out = format_log(&log);
    assert!(out.contains("12:34:56"));
    assert!(out.contains("INFO"));
    assert!(out.contains("app"));
    assert!(out.contains("hello"));
}

#[test]
fn format_log_aligns_continuation_lines_under_message_column() {
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "app".to_string(),
        message: "line1\nline2\nline3".to_string(),
        fields: None,
        kind: None,
    };
    let rendered = format_log(&log);
    let plain = strip_ansi(&rendered);
    let lines: Vec<&str> = plain.split('\n').collect();
    assert_eq!(lines.len(), 3);

    let first = lines[0];
    let msg_col = first.find("line1").expect("first line contains msg");

    assert!(lines[1].starts_with(&" ".repeat(msg_col)));
    assert_eq!(&lines[1][msg_col..], "line2");
    assert!(lines[2].starts_with(&" ".repeat(msg_col)));
    assert_eq!(&lines[2][msg_col..], "line3");
}

#[test]
fn format_log_wraps_long_lines_under_message_column() {
    let long_path = "/Users/dan/github/repobouncer/node_modules/tako.sh/package.json";
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Error,
        scope: "vite".to_string(),
        message: format!(r#"Error: "./runtime" is not exported from package {long_path}"#),
        fields: None,
        kind: None,
    };
    let plain = strip_ansi(&format_log_for_width(&log, 78));
    let lines: Vec<&str> = plain.split('\n').collect();
    assert!(lines.len() > 1, "expected wrapping, got {plain:?}");

    let msg_col = lines[0]
        .find("Error:")
        .expect("first line contains message");
    for line in lines.iter().skip(1) {
        assert!(
            line.starts_with(&" ".repeat(msg_col)),
            "wrapped line should align under message column: {line:?}"
        );
        assert!(
            measure_text_width(line) <= 78,
            "renderer should wrap before terminal auto-wraps: {line:?}"
        );
    }
}

#[test]
fn raw_terminal_block_resets_carriage_for_each_multiline_row() {
    let block = raw_terminal_block("alpha\nbeta\n  gamma");
    assert_eq!(block, "\ralpha\r\n\rbeta\r\n\r  gamma\r\n");
}

#[test]
fn format_log_appends_fields_as_key_value_suffix() {
    let mut fields = serde_json::Map::new();
    fields.insert("step".to_string(), serde_json::json!("fetch"));
    fields.insert("ms".to_string(), serde_json::json!(24));
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "worker:broadcast".to_string(),
        message: "Step completed".to_string(),
        fields: Some(fields),
        kind: None,
    };
    let rendered = format_log(&log);
    assert!(rendered.contains("\x1b[2;3m"));
    let plain = strip_ansi(&rendered);
    assert!(plain.contains("Step completed"));
    assert!(plain.contains("step=fetch"));
    assert!(plain.contains("ms=24"));
}

#[test]
fn format_log_splits_compound_scope_with_dim_separator() {
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "worker:broadcast".to_string(),
        message: "Sleep completed".to_string(),
        fields: None,
        kind: None,
    };
    let rendered = format_log(&log);
    let scope_start = rendered.find("\x1b[38;2;").expect("colored scope present");
    let scope_section = &rendered[scope_start..];
    let dim_idx = scope_section
        .find("\x1b[2m:\x1b[0m")
        .expect("dim `:` separator");
    let prefix_color_end = scope_section[..dim_idx].rfind('m').unwrap();
    let prefix_color = &scope_section[..=prefix_color_end];
    let after_sep = &scope_section[dim_idx + "\x1b[2m:\x1b[0m".len()..];
    let suffix_color_end = after_sep.find('m').unwrap();
    let suffix_color = &after_sep[..=suffix_color_end];
    assert_ne!(prefix_color, suffix_color, "prefix and suffix must differ");
    assert!(strip_ansi(&rendered).contains("worker:broadcast"));
}

#[test]
fn format_log_renders_error_object_as_message_only() {
    let mut fields = serde_json::Map::new();
    fields.insert(
        "error".to_string(),
        serde_json::json!({ "name": "Error", "message": "boom", "stack": "long\nstack\ntrace" }),
    );
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Error,
        scope: "worker:broadcast".to_string(),
        message: "Run failed".to_string(),
        fields: Some(fields),
        kind: None,
    };
    let plain = strip_ansi(&format_log(&log));
    assert!(plain.contains("error=boom"));
    assert!(!plain.contains("stack"));
}

#[test]
fn format_log_skips_build_and_instance_globals() {
    let mut fields = serde_json::Map::new();
    fields.insert("build".to_string(), serde_json::json!("abc"));
    fields.insert("instance".to_string(), serde_json::json!("i1"));
    fields.insert("runId".to_string(), serde_json::json!("t1"));
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "worker:broadcast".to_string(),
        message: "Run started".to_string(),
        fields: Some(fields),
        kind: None,
    };
    let plain = strip_ansi(&format_log(&log));
    assert!(plain.contains("runId=t1"));
    assert!(!plain.contains("build="));
    assert!(!plain.contains("instance="));
}

#[test]
fn format_log_single_line_message_unaffected() {
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "app".to_string(),
        message: "no newline".to_string(),
        fields: None,
        kind: None,
    };
    let out = format_log(&log);
    assert!(!out.contains('\n'));
    assert!(out.contains("no newline"));
}

#[test]
fn fit_scope_pads_short_scopes() {
    assert_eq!(fit_scope("app"), "app ");
    assert_eq!(fit_scope("up"), "up  ");
}

#[test]
fn fit_scope_keeps_exact_min() {
    assert_eq!(fit_scope("tako"), "tako");
}

#[test]
fn fit_scope_keeps_mid_length() {
    assert_eq!(fit_scope("myservice"), "myservice");
}

#[test]
fn fit_scope_truncates_long_scopes() {
    assert_eq!(
        fit_scope("worker:really-long-workflow-names"),
        "worker:really-long-workflow-nam\u{2026}",
    );
}

#[test]
fn format_log_renders_kind_as_divider_with_humanized_label() {
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "tako".to_string(),
        message: String::new(),
        fields: None,
        kind: Some("lan_mode_enabled".to_string()),
    };
    let plain = strip_ansi(&format_log(&log));
    assert!(
        plain.contains("────"),
        "expected divider rule, got: {plain:?}"
    );
    assert!(
        plain.contains("lan mode enabled"),
        "underscores should become spaces, got: {plain:?}"
    );
    assert!(
        !plain.contains("INFO"),
        "divider should not carry log columns, got: {plain:?}"
    );
    assert!(
        !plain.contains("12:34:56"),
        "divider should omit timestamp, got: {plain:?}"
    );
}

#[test]
fn format_log_without_kind_stays_a_normal_log_line() {
    let log = ScopedLog {
        timestamp: "12:34:56".to_string(),
        level: LogLevel::Info,
        scope: "tako".to_string(),
        message: "restarted".to_string(),
        fields: None,
        kind: None,
    };
    let plain = strip_ansi(&format_log(&log));
    assert!(
        !plain.contains("────"),
        "plain log must not render as divider: {plain:?}"
    );
    assert!(plain.contains("INFO"));
    assert!(plain.contains("tako"));
}

#[test]
fn parse_log_line_preserves_kind_from_wire_format() {
    use crate::commands::dev::client::parse_log_line;
    let line = r#"{"ts":1700000000000,"level":"info","scope":"tako","kind":"restarted"}"#;
    let log = parse_log_line(line).expect("kind-only line parses");
    assert_eq!(log.scope, "tako");
    assert_eq!(log.kind.as_deref(), Some("restarted"));
    assert_eq!(log.message, "", "msg is optional when kind is set");
}
