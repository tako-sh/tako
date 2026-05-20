use crate::commands::log_style::{
    DIM, RESET, render_app_process_scope, render_compact_scope, render_metadata_suffix,
};

mod stream;

pub(super) use stream::LogWriter;
use stream::coalesce_raw_app_blocks;

pub(super) fn format_and_dedup(
    lines: &[(String, String)],
    app_name: &str,
    show_prefix: bool,
    colorize: bool,
) -> String {
    let mut out = String::new();
    let mut last_key = String::new();
    let mut repeat_count: u32 = 0;
    let mut last_repeat_indent: usize = 0;
    let mut last_repeat_start: Option<String> = None;
    let mut last_repeat_through: Option<String> = None;

    let logical_lines = coalesce_raw_app_blocks(lines);
    for (server, raw) in &logical_lines {
        let entry = render_log_entry(raw, app_name, colorize);
        if !entry.key.is_empty() && entry.key == last_key {
            repeat_count += 1;
            last_repeat_through.clone_from(&entry.repeat_timestamp);
        } else {
            push_repeat(
                &mut out,
                repeat_count,
                last_repeat_indent,
                last_repeat_start.as_deref(),
                last_repeat_through.as_deref(),
                colorize,
            );
            let prefix = format_prefix(server, show_prefix, colorize);
            out.push_str(&prefix);
            out.push_str(&entry.formatted);
            out.push('\n');
            last_key = entry.key;
            last_repeat_indent = prefix_width(server, show_prefix) + entry.repeat_indent;
            last_repeat_start = entry.repeat_timestamp;
            last_repeat_through = None;
            repeat_count = 0;
        }
    }
    push_repeat(
        &mut out,
        repeat_count,
        last_repeat_indent,
        last_repeat_start.as_deref(),
        last_repeat_through.as_deref(),
        colorize,
    );
    out
}

fn push_repeat(
    out: &mut String,
    count: u32,
    indent: usize,
    start: Option<&str>,
    through: Option<&str>,
    colorize: bool,
) {
    if count > 0 {
        let marker = repeat_marker(count, start, through);
        if colorize {
            out.push_str(&format!("{}{DIM}{marker}{RESET}\n", " ".repeat(indent)));
        } else {
            out.push_str(&format!("{}{marker}\n", " ".repeat(indent)));
        }
    }
}

fn repeat_marker(duplicate_count: u32, start: Option<&str>, through: Option<&str>) -> String {
    let total = duplicate_count + 1;
    match through {
        Some(through) => format!(
            "└─ repeated {total} times through {}",
            compact_repeat_timestamp(start, through)
        ),
        None => format!("└─ repeated {total} times"),
    }
}

fn compact_repeat_timestamp(start: Option<&str>, through: &str) -> String {
    if let Some(start) = start
        && start.len() >= 19
        && through.len() >= 19
        && start[..10] == through[..10]
    {
        return through[11..19].to_string();
    }
    through.to_string()
}

pub(super) fn format_prefix(server: &str, show: bool, colorize: bool) -> String {
    if !show {
        return String::new();
    }
    if colorize {
        format!("{DIM}[{server}]{RESET} ")
    } else {
        format!("[{server}] ")
    }
}

fn prefix_width(server: &str, show: bool) -> usize {
    if show { server.len() + 3 } else { 0 }
}

#[cfg(test)]
fn format_log_entry(line: &str, colorize: bool) -> (String, String) {
    let entry = render_log_entry(line, "demo", colorize);
    (entry.key, entry.formatted)
}

struct RenderedLogEntry {
    key: String,
    formatted: String,
    repeat_indent: usize,
    repeat_timestamp: Option<String>,
}

fn render_log_entry(line: &str, app_name: &str, colorize: bool) -> RenderedLogEntry {
    if let Some((hms, level, message)) = parse_json_log(line) {
        let key = format!("{level} {message}");
        let formatted = format_level_row(&hms, &level, None, None, &message, colorize);
        RenderedLogEntry {
            key,
            formatted,
            repeat_indent: repeat_indent_for_message(&hms, None, &message),
            repeat_timestamp: Some(hms),
        }
    } else if let Some(entry) = parse_app_log(line, app_name) {
        let key = format!("{} {} {}", entry.level, entry.scope, entry.message);
        let formatted = format_level_row(
            &entry.timestamp,
            &entry.level,
            Some(&entry.scope),
            entry.app_process_scope.as_deref(),
            &entry.message,
            colorize,
        );
        RenderedLogEntry {
            key,
            formatted,
            repeat_indent: repeat_indent_for_message(
                &entry.timestamp,
                Some(&entry.scope),
                &entry.message,
            ),
            repeat_timestamp: Some(entry.timestamp),
        }
    } else {
        RenderedLogEntry {
            key: String::new(),
            formatted: line.to_string(),
            repeat_indent: 0,
            repeat_timestamp: None,
        }
    }
}

struct AppLogEntry {
    timestamp: String,
    level: String,
    scope: String,
    app_process_scope: Option<String>,
    message: String,
}

fn format_level_row(
    timestamp: &str,
    level: &str,
    scope: Option<&str>,
    app_process_scope: Option<&str>,
    message: &str,
    colorize: bool,
) -> String {
    let level_text = if colorize {
        let color = level_color(level);
        if color.is_empty() {
            format!("{level:>5}")
        } else {
            format!("{color}{level:>5}{RESET}")
        }
    } else {
        format!("{level:>5}")
    };
    let timestamp_text = if colorize {
        format!("{DIM}{timestamp}{RESET}")
    } else {
        timestamp.to_string()
    };

    let mut lines = message.split('\n');
    let first = lines.next().unwrap_or_default();
    let mut out = match scope {
        Some(scope) => {
            let scope_text = format_scope_column(scope, app_process_scope, colorize);
            format!(
                "{timestamp_text} {level_text} {scope_text} {}",
                format_message_line(first, colorize)
            )
        }
        None => format!(
            "{timestamp_text} {level_text} {}",
            format_message_line(first, colorize)
        ),
    };

    let continuation_width = message_column_width(timestamp, scope);
    for line in lines {
        out.push('\n');
        out.push_str(&" ".repeat(continuation_width));
        out.push_str(line);
    }

    out
}

fn message_column_width(timestamp: &str, scope: Option<&str>) -> usize {
    // timestamp + space + right-aligned level + space + optional scope + message separator
    let base = timestamp.len() + 1 + 5 + 1;
    match scope {
        Some(scope) => base + scope.len() + 1,
        None => base,
    }
}

fn repeat_indent_for_message(timestamp: &str, scope: Option<&str>, message: &str) -> usize {
    let base = message_column_width(timestamp, scope);
    let continuation_indent = message
        .split('\n')
        .skip(1)
        .find(|line| !line.is_empty())
        .map(leading_whitespace_width)
        .unwrap_or(0);
    base + continuation_indent
}

fn leading_whitespace_width(line: &str) -> usize {
    line.chars()
        .take_while(|ch| ch.is_whitespace())
        .map(|ch| if ch == '\t' { 4 } else { 1 })
        .sum()
}

fn format_scope_column(scope: &str, app_process_scope: Option<&str>, colorize: bool) -> String {
    if !colorize {
        return scope.to_string();
    }

    scope
        .split(' ')
        .map(|part| {
            if Some(part) == app_process_scope {
                render_app_process_scope(part)
            } else {
                render_compact_scope(part)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_message_line(line: &str, colorize: bool) -> String {
    let Some((message, fields)) = split_trailing_metadata_fields(line) else {
        return line.to_string();
    };
    format!("{message}{}", render_metadata_suffix(fields, colorize))
}

fn split_trailing_metadata_fields(line: &str) -> Option<(&str, &str)> {
    let tokens: Vec<_> = line.split(' ').collect();
    let split = tokens
        .iter()
        .rposition(|token| !is_metadata_token(token))
        .map(|idx| idx + 1)
        .unwrap_or(0);
    if split == 0 || split == tokens.len() {
        return None;
    }

    let message_len = tokens[..split].join(" ").len();
    let fields = line[message_len..].trim_start();
    Some((&line[..message_len], fields))
}

fn is_metadata_token(token: &str) -> bool {
    let Some((key, value)) = token.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && !value.is_empty()
        && key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
}

fn parse_app_log(line: &str, app_name: &str) -> Option<AppLogEntry> {
    let parts = parse_app_log_parts(line)?;
    let timestamp = format_timestamp(parts.timestamp);

    let mut entry = AppLogEntry {
        timestamp,
        level: app_stream_level(parts.stream).to_string(),
        scope: app_log_scope(app_name, parts.stream, parts.instance),
        app_process_scope: (parts.stream != "server").then(|| parts.instance.to_string()),
        message: parts.message.to_string(),
    };

    if let Some(structured) = parse_structured_app_message(parts.message) {
        entry.level = structured.level;
        if let Some(scope) = structured.scope {
            entry.scope = format!("{} {}", entry.scope, scope);
        }
        entry.message = structured.message;
    } else if parts.stream == "server"
        && let Some((level, message)) = split_prefixed_level(parts.message)
    {
        entry.level = level.to_string();
        entry.message = message.to_string();
    }

    Some(entry)
}

fn app_log_scope(_app_name: &str, stream: &str, instance: &str) -> String {
    if stream == "server" {
        server_log_scope(instance).to_string()
    } else {
        instance.to_string()
    }
}

fn server_log_scope(instance: &str) -> &str {
    match instance {
        "tako-server" => "tako",
        source => source,
    }
}

struct AppLogParts<'a> {
    timestamp: &'a str,
    stream: &'a str,
    instance: &'a str,
    message: &'a str,
}

fn parse_app_log_parts(line: &str) -> Option<AppLogParts<'_>> {
    let (timestamp, rest) = line.split_once(' ')?;
    if timestamp.len() < 19 || !timestamp.contains('T') {
        return None;
    }

    let (stream, rest) = parse_bracketed(rest)?;
    let (instance, message) = parse_bracketed(strip_log_separator(rest))?;
    let message = strip_log_separator(message);

    Some(AppLogParts {
        timestamp,
        stream,
        instance,
        message,
    })
}

fn strip_log_separator(rest: &str) -> &str {
    rest.strip_prefix(' ').unwrap_or(rest)
}

struct StructuredAppMessage {
    level: String,
    scope: Option<String>,
    message: String,
}

fn parse_structured_app_message(message: &str) -> Option<StructuredAppMessage> {
    let value: serde_json::Value = serde_json::from_str(message).ok()?;
    let object = value.as_object()?;
    let level = object
        .get("level")
        .and_then(|value| value.as_str())
        .map(normalize_level)
        .unwrap_or_else(|| "INFO".to_string());
    let msg = object
        .get("msg")
        .and_then(|value| value.as_str())
        .unwrap_or(message);
    let scope = object
        .get("scope")
        .and_then(|value| value.as_str())
        .filter(|scope| !scope.is_empty())
        .map(str::to_string);

    let mut message = msg.to_string();

    if let Some(fields) = object.get("fields").and_then(|value| value.as_object()) {
        if let Some(stack) = fields.get("error").and_then(error_stack) {
            message = append_error_stack(message, stack);
        }

        let mut field_parts: Vec<_> = fields
            .iter()
            .filter(|(key, _value)| key.as_str() != "error")
            .map(|(key, value)| format!("{key}={}", render_log_value(value)))
            .collect();
        field_parts.sort();
        message = append_fields_to_first_line(message, &field_parts);
    }

    Some(StructuredAppMessage {
        level,
        scope,
        message,
    })
}

fn error_stack(value: &serde_json::Value) -> Option<&str> {
    value
        .as_object()
        .and_then(|object| object.get("stack"))
        .and_then(|value| value.as_str())
        .filter(|stack| !stack.is_empty())
}

fn append_error_stack(message: String, stack: &str) -> String {
    let stack = stack.trim_end();
    if stack.is_empty() || message == stack || message.contains(stack) {
        return message;
    }

    let first_message_line = message.lines().next().unwrap_or_default();
    let mut stack_lines = stack.lines();
    let first_stack_line = stack_lines.next().unwrap_or_default();
    let lines: Vec<_> = if first_message_line == first_stack_line
        || first_message_line.ends_with(first_stack_line)
    {
        stack_lines.collect()
    } else {
        stack.lines().collect()
    };

    if lines.is_empty() {
        message
    } else {
        format!("{message}\n{}", lines.join("\n"))
    }
}

fn append_fields_to_first_line(message: String, fields: &[String]) -> String {
    if fields.is_empty() {
        return message;
    }

    let suffix = format!(" {}", fields.join(" "));
    match message.split_once('\n') {
        Some((first, rest)) => format!("{first}{suffix}\n{rest}"),
        None => format!("{message}{suffix}"),
    }
}

fn parse_bracketed(input: &str) -> Option<(&str, &str)> {
    let rest = input.strip_prefix('[')?;
    let (value, rest) = rest.split_once(']')?;
    Some((value, rest))
}

fn app_stream_level(stream: &str) -> &'static str {
    match stream {
        "err" => "ERROR",
        _ => "INFO",
    }
}

fn split_prefixed_level(message: &str) -> Option<(&'static str, &str)> {
    for level in ["ERROR", "WARN", "INFO", "DEBUG", "TRACE", "FATAL"] {
        if let Some(rest) = message
            .strip_prefix(level)
            .and_then(|rest| rest.strip_prefix(' '))
        {
            return Some((level, rest));
        }
    }
    None
}

fn normalize_level(level: &str) -> String {
    match level.to_ascii_uppercase().as_str() {
        "WARNING" => "WARN".to_string(),
        other => other.to_string(),
    }
}

fn render_log_value(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

/// Parse a JSON log line from tracing-subscriber `.json()` format.
///
/// Expected: `{"timestamp":"...","level":"INFO","fields":{"message":"...","app":"..."}}`
fn parse_json_log(line: &str) -> Option<(String, String, String)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let timestamp = v["timestamp"].as_str()?;
    let level = v["level"].as_str()?;
    let fields = v.get("fields")?.as_object()?;
    let message = fields.get("message").and_then(|m| m.as_str()).unwrap_or("");

    let mut parts = vec![message.to_string()];
    for (k, val) in fields {
        if k == "message" {
            continue;
        }
        let v_str = val
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| val.to_string());
        parts.push(format!("{k}={v_str}"));
    }

    let hms = format_timestamp(timestamp);

    Some((hms, level.to_string(), parts.join(" ")))
}

fn format_timestamp(timestamp: &str) -> String {
    if timestamp.len() >= 19 {
        format!("{} {}", &timestamp[..10], &timestamp[11..19])
    } else {
        timestamp.to_string()
    }
}

pub(super) fn extract_timestamp(line: &str) -> &str {
    if let Some(pos) = line.find("\"timestamp\":\"") {
        let start = pos + 13;
        if let Some(end) = line[start..].find('"') {
            return &line[start..start + end];
        }
    }
    if line.len() >= 24 && line.as_bytes()[4] == b'-' && line.as_bytes()[10] == b'T' {
        return &line[..24];
    }
    "\x7f"
}

fn level_color(level: &str) -> &'static str {
    match level {
        "DEBUG" | "TRACE" => "\x1b[38;2;140;207;255m",
        "INFO" => "\x1b[38;2;155;217;179m",
        "WARN" => "\x1b[38;2;234;211;156m",
        "ERROR" => "\x1b[38;2;232;163;160m",
        "FATAL" => "\x1b[38;2;200;166;242m",
        _ => "",
    }
}

#[cfg(test)]
mod tests;
