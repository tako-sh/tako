use super::*;

#[test]
fn parse_json_log_info() {
    let line = r#"{"timestamp":"2026-03-10T12:34:56.789012Z","level":"INFO","fields":{"message":"Instance is healthy","app":"bun-example","instance":"abc123"}}"#;
    let (hms, level, msg) = parse_json_log(line).unwrap();
    assert_eq!(hms, "2026-03-10 12:34:56");
    assert_eq!(level, "INFO");
    assert!(msg.contains("Instance is healthy"));
    assert!(msg.contains("app=bun-example"));
    assert!(msg.contains("instance=abc123"));
}

#[test]
fn parse_json_log_warn() {
    let line = r#"{"timestamp":"2026-03-10T08:00:00.000Z","level":"WARN","fields":{"message":"timeout","app":"foo"}}"#;
    let (hms, level, msg) = parse_json_log(line).unwrap();
    assert_eq!(hms, "2026-03-10 08:00:00");
    assert_eq!(level, "WARN");
    assert!(msg.starts_with("timeout"));
    assert!(msg.contains("app=foo"));
}

#[test]
fn parse_json_log_non_json() {
    assert!(parse_json_log("just some random text").is_none());
    assert!(parse_json_log("").is_none());
}

#[test]
fn format_app_stderr_log_line_as_error_row() {
    let line = "2026-05-08T06:02:58.800Z [err] [5WHq7f05] error: Expected a Response object";

    let (key, formatted) = format_log_entry(line, false);

    assert_eq!(
        formatted,
        "2026-05-08 06:02:58 ERROR 5WHq7f05 error: Expected a Response object"
    );
    assert_eq!(key, "ERROR 5WHq7f05 error: Expected a Response object");
}

#[test]
fn preserves_app_log_message_leading_spaces_after_separator() {
    let line = "2026-05-08T06:02:58.800Z [err] [5WHq7f05]   status: [Getter],";

    let (_key, formatted) = format_log_entry(line, false);

    assert_eq!(
        formatted,
        "2026-05-08 06:02:58 ERROR 5WHq7f05   status: [Getter],"
    );
}

#[test]
fn colorize_app_stderr_log_line() {
    let line = "2026-05-08T06:02:58.800Z [err] [5WHq7f05] boom";

    let (_key, formatted) = format_log_entry(line, true);

    assert!(formatted.contains("\x1b[2m2026-05-08 06:02:58\x1b[0m"));
    assert!(formatted.contains("\x1b[38;2;232;163;160mERROR\x1b[0m"));
    assert!(formatted.contains("5WHq7f05"));
    assert!(formatted.contains(" boom"));
}

#[test]
fn colorize_scope_column_without_changing_plain_text() {
    let line = r#"2026-04-20T06:19:53.716Z [out] [vdACgHcC] {"level":"info","scope":"worker:broadcast","msg":"queued"}"#;

    let (_key, formatted) = format_log_entry(line, true);

    assert!(formatted.contains("\x1b[2m:\x1b[0m"));
    assert_eq!(
        console::strip_ansi_codes(&formatted),
        "2026-04-20 06:19:53  INFO vdACgHcC worker:broadcast queued"
    );
}

#[test]
fn format_structured_app_log_line() {
    let line = r#"2026-04-20T06:19:53.716Z [out] [vdACgHcC] {"level":"error","scope":"sdk.rpc","msg":"rpc rejected","fields":{"code":"TAKO_RPC_ERROR"}}"#;

    let (key, formatted) = format_log_entry(line, false);

    assert_eq!(
        formatted,
        "2026-04-20 06:19:53 ERROR vdACgHcC sdk.rpc rpc rejected code=TAKO_RPC_ERROR"
    );
    assert_eq!(
        key,
        "ERROR vdACgHcC sdk.rpc rpc rejected code=TAKO_RPC_ERROR"
    );
}

#[test]
fn format_structured_app_multiline_log_as_one_entry() {
    let line = r#"2026-05-08T06:02:58.800Z [out] [5WHq7f05] {"level":"error","scope":"app","msg":"Error: boom\n    at foo (x.ts:1:1)\n    at bar (y.ts:2:2)"}"#;

    let (_key, formatted) = format_log_entry(line, false);

    let lines: Vec<_> = formatted.lines().collect();
    let continuation = " ".repeat(message_column_width(
        "2026-05-08 06:02:58",
        Some("5WHq7f05 app"),
    ));
    assert_eq!(
        lines[0],
        "2026-05-08 06:02:58 ERROR 5WHq7f05 app Error: boom"
    );
    assert_eq!(lines[1], format!("{continuation}    at foo (x.ts:1:1)"));
    assert_eq!(lines[2], format!("{continuation}    at bar (y.ts:2:2)"));
}

#[test]
fn format_structured_app_error_field_stack_as_continuation() {
    let line = r#"2026-05-08T06:02:58.800Z [out] [5WHq7f05] {"level":"error","scope":"app","msg":"Error in user fetch handler: Error: boom","fields":{"error":{"name":"Error","message":"boom","stack":"Error: boom\n    at foo (x.ts:1:1)\n    at bar (y.ts:2:2)"},"kind":"fetch"}}"#;

    let (_key, formatted) = format_log_entry(line, false);

    let lines: Vec<_> = formatted.lines().collect();
    let continuation = " ".repeat(message_column_width(
        "2026-05-08 06:02:58",
        Some("5WHq7f05 app"),
    ));
    assert_eq!(
        lines[0],
        "2026-05-08 06:02:58 ERROR 5WHq7f05 app Error in user fetch handler: Error: boom kind=fetch"
    );
    assert_eq!(lines[1], format!("{continuation}    at foo (x.ts:1:1)"));
    assert_eq!(lines[2], format!("{continuation}    at bar (y.ts:2:2)"));
}

#[test]
fn format_structured_app_error_field_does_not_duplicate_stack_in_msg() {
    let line = r#"2026-05-08T06:02:58.800Z [out] [5WHq7f05] {"level":"error","scope":"app","msg":"Error: boom\n    at foo (x.ts:1:1)","fields":{"error":{"name":"Error","message":"boom","stack":"Error: boom\n    at foo (x.ts:1:1)"},"kind":"uncaughtException"}}"#;

    let (_key, formatted) = format_log_entry(line, false);

    let lines: Vec<_> = formatted.lines().collect();
    let continuation = " ".repeat(message_column_width(
        "2026-05-08 06:02:58",
        Some("5WHq7f05 app"),
    ));
    assert_eq!(
        lines[0],
        "2026-05-08 06:02:58 ERROR 5WHq7f05 app Error: boom kind=uncaughtException"
    );
    assert_eq!(lines[1], format!("{continuation}    at foo (x.ts:1:1)"));
    assert_eq!(lines.len(), 2);
}

#[test]
fn group_raw_app_object_dump_lines_as_one_entry() {
    let lines = vec![
        (
            "prod".to_string(),
            "2026-05-08T06:02:58.800Z [err] [5WHq7f05] error: Expected a Response object, but received 'NodeResponse {".to_string(),
        ),
        (
            "prod".to_string(),
            "2026-05-08T06:02:58.865Z [err] [5WHq7f05]   status: [Getter],".to_string(),
        ),
        (
            "prod".to_string(),
            "2026-05-08T06:02:58.899Z [err] [5WHq7f05]   }'".to_string(),
        ),
    ];

    let output = format_and_dedup(&lines, "demo", false, false);
    let rows: Vec<_> = output.trim_end().lines().collect();
    let continuation = " ".repeat(message_column_width(
        "2026-05-08 06:02:58",
        Some("5WHq7f05"),
    ));

    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0],
        "2026-05-08 06:02:58 ERROR 5WHq7f05 error: Expected a Response object, but received 'NodeResponse {"
    );
    assert_eq!(rows[1], format!("{continuation}  status: [Getter],"));
    assert_eq!(rows[2], format!("{continuation}  }}'"));
}

#[test]
fn dedup_repeated_raw_app_object_dumps_after_grouping() {
    let lines = vec![
        (
            "prod".to_string(),
            "2026-05-08T06:02:58.800Z [err] [5WHq7f05] error: Expected a Response object, but received 'NodeResponse {".to_string(),
        ),
        (
            "prod".to_string(),
            "2026-05-08T06:02:58.865Z [err] [5WHq7f05]   status: [Getter],".to_string(),
        ),
        (
            "prod".to_string(),
            "2026-05-08T06:02:58.899Z [err] [5WHq7f05]   }'".to_string(),
        ),
        (
            "prod".to_string(),
            "2026-05-08T06:02:59.241Z [err] [5WHq7f05] error: Expected a Response object, but received 'NodeResponse {".to_string(),
        ),
        (
            "prod".to_string(),
            "2026-05-08T06:02:59.241Z [err] [5WHq7f05]   status: [Getter],".to_string(),
        ),
        (
            "prod".to_string(),
            "2026-05-08T06:02:59.242Z [err] [5WHq7f05]   }'".to_string(),
        ),
    ];

    let output = format_and_dedup(&lines, "demo", false, false);
    let rows: Vec<_> = output.trim_end().lines().collect();
    let repeat = " ".repeat(repeat_indent_for_message(
        "2026-05-08 06:02:58",
        Some("5WHq7f05"),
        "error: Expected a Response object, but received 'NodeResponse {\n  status: [Getter],\n  }'",
    ));

    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows[0],
        "2026-05-08 06:02:58 ERROR 5WHq7f05 error: Expected a Response object, but received 'NodeResponse {"
    );
    assert!(rows[1].ends_with("  status: [Getter],"));
    assert!(rows[2].ends_with("  }'"));
    assert_eq!(
        rows[3],
        format!("{repeat}└─ repeated 2 times through 06:02:59")
    );
}

#[test]
fn format_app_scoped_server_log_line() {
    let line =
        "2026-05-08T07:26:50.851Z [server] [tako-server] INFO Instance ready instance=zF-c2auM";

    let (_key, formatted) = format_log_entry(line, false);

    assert_eq!(
        formatted,
        "2026-05-08 07:26:50  INFO tako Instance ready instance=zF-c2auM"
    );
}

#[test]
fn colorized_server_log_dims_metadata_fields() {
    let line =
        "2026-05-08T07:26:50.851Z [server] [tako-server] INFO Instance ready instance=zF-c2auM";

    let (_key, formatted) = format_log_entry(line, true);

    assert!(formatted.contains("\x1b[2;3minstance=zF-c2auM\x1b[0m"));
    assert_eq!(
        console::strip_ansi_codes(&formatted),
        "2026-05-08 07:26:50  INFO tako Instance ready instance=zF-c2auM"
    );
}

#[test]
fn dedup_consecutive_lines() {
    let lines = vec![
        (
            "s1".to_string(),
            r#"{"timestamp":"2026-03-10T12:00:00.000Z","level":"INFO","fields":{"message":"hello","app":"x"}}"#.to_string(),
        ),
        (
            "s1".to_string(),
            r#"{"timestamp":"2026-03-10T12:00:01.000Z","level":"INFO","fields":{"message":"hello","app":"x"}}"#.to_string(),
        ),
        (
            "s1".to_string(),
            r#"{"timestamp":"2026-03-10T12:00:02.000Z","level":"INFO","fields":{"message":"hello","app":"x"}}"#.to_string(),
        ),
        (
            "s1".to_string(),
            r#"{"timestamp":"2026-03-10T12:00:03.000Z","level":"WARN","fields":{"message":"different","app":"x"}}"#.to_string(),
        ),
    ];
    let output = format_and_dedup(&lines, "demo", false, false);
    let result: Vec<&str> = output.trim().lines().collect();
    assert_eq!(result.len(), 3);
    assert!(result[0].contains("hello"));
    assert!(result[1].contains("└─ repeated 3 times through 12:00:02"));
    assert!(result[2].contains("different"));
}

#[test]
fn extract_timestamp_from_json() {
    let line =
        r#"{"timestamp":"2026-03-10T12:34:56.789Z","level":"INFO","fields":{"message":"hi"}}"#;
    assert_eq!(extract_timestamp(line), "2026-03-10T12:34:56.789Z");
}

#[test]
fn extract_timestamp_from_app_log() {
    let line = "2026-04-03T12:00:00.000Z [out] [inst-1] hello world";
    assert_eq!(extract_timestamp(line), "2026-04-03T12:00:00.000Z");
}

#[test]
fn extract_timestamp_non_json() {
    assert_eq!(extract_timestamp("random text"), "\x7f");
}

#[test]
fn sort_by_timestamp() {
    let a =
        r#"{"timestamp":"2026-03-10T12:00:02.000Z","level":"INFO","fields":{"message":"second"}}"#;
    let b =
        r#"{"timestamp":"2026-03-10T12:00:01.000Z","level":"INFO","fields":{"message":"first"}}"#;
    assert!(extract_timestamp(b) < extract_timestamp(a));
}
