use super::super::{LogLevel, ScopedLog};
use super::shared::{DIM, RESET, SCOPE_MAX, SCOPE_MIN, muted, terminal_cols, vlen};
use crate::commands::log_style::{format_json_fields_suffix, render_scope};

pub(in crate::commands::dev) use crate::commands::log_style::set_app_runtime;

pub(in crate::commands::dev) fn fit_scope(scope: &str) -> String {
    let len = scope.len();
    if len <= SCOPE_MAX {
        format!("{scope:<SCOPE_MIN$}")
    } else {
        format!("{}\u{2026}", &scope[..SCOPE_MAX - 1])
    }
}

pub(in crate::commands::dev) fn format_log(log: &ScopedLog) -> String {
    format_log_for_width(log, terminal_cols())
}

pub(in crate::commands::dev) fn format_log_for_width(log: &ScopedLog, cols: usize) -> String {
    if let Some(kind) = log.kind.as_deref() {
        let label = kind.replace('_', " ");
        return muted(&format!("──── {label} ────"));
    }
    let fields_suffix = format_json_fields_suffix(log.fields.as_ref());
    if matches!(log.level, LogLevel::Debug) {
        let scope = fit_scope(&log.scope);
        let rendered_scope = render_scope(&log.scope, &scope);
        let pad_width = message_column_width(scope.len());
        let mut lines = log.message.split('\n');
        let first = lines.next().unwrap_or("");
        let wrapped_first = wrap_message_line(first, fields_suffix.as_str(), pad_width, cols);
        let mut out = format!(
            "{DIM}{} {:>5}{RESET} {rendered_scope} {DIM}{}{RESET}",
            log.timestamp, log.level, wrapped_first[0]
        );
        for line in wrapped_first.iter().skip(1) {
            out.push('\n');
            out.push_str(&" ".repeat(pad_width));
            out.push_str(&format!("{DIM}{line}{RESET}"));
        }
        for line in lines {
            for wrapped in wrap_message_line(line, "", pad_width, cols) {
                out.push('\n');
                out.push_str(&" ".repeat(pad_width));
                out.push_str(&format!("{DIM}{wrapped}{RESET}"));
            }
        }
        return out;
    }
    let color = level_color(&log.level);
    let scope = fit_scope(&log.scope);
    let rendered_scope = render_scope(&log.scope, &scope);
    let pad_width = message_column_width(scope.len());
    let mut lines = log.message.split('\n');
    let first = lines.next().unwrap_or("");
    let wrapped_first = wrap_message_line(first, fields_suffix.as_str(), pad_width, cols);
    let mut out = format!(
        "{DIM}{}{RESET} {color}{:>5}{RESET} {rendered_scope} {}",
        log.timestamp, log.level, wrapped_first[0]
    );
    for line in wrapped_first.iter().skip(1) {
        out.push('\n');
        out.push_str(&" ".repeat(pad_width));
        out.push_str(line);
    }
    for line in lines {
        for wrapped in wrap_message_line(line, "", pad_width, cols) {
            out.push('\n');
            out.push_str(&" ".repeat(pad_width));
            out.push_str(&wrapped);
        }
    }
    out
}

fn wrap_message_line(line: &str, suffix: &str, pad_width: usize, cols: usize) -> Vec<String> {
    let width = cols.saturating_sub(pad_width).max(20);
    let mut wrapped = wrap_visible(line, width);
    if suffix.is_empty() {
        return wrapped;
    }

    if let Some(last) = wrapped.last_mut()
        && vlen(last) + vlen(suffix) <= width
    {
        last.push_str(suffix);
        return wrapped;
    }

    wrapped.push(suffix.to_string());
    wrapped
}

fn wrap_visible(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }

    let mut out = Vec::new();
    let mut remaining = line;
    while vlen(remaining) > width {
        let split = split_visible_prefix(remaining, width);
        let split = split_at_last_space(&remaining[..split]).unwrap_or(split);
        let (head, tail) = remaining.split_at(split);
        out.push(head.trim_end().to_string());
        remaining = tail.trim_start();
        if remaining.is_empty() {
            break;
        }
    }
    if !remaining.is_empty() {
        out.push(remaining.to_string());
    }
    out
}

fn split_visible_prefix(s: &str, max_width: usize) -> usize {
    let mut width = 0;
    let mut last = 0;
    for (idx, ch) in s.char_indices() {
        let ch_width = vlen(ch.encode_utf8(&mut [0; 4]));
        if width + ch_width > max_width {
            break;
        }
        width += ch_width;
        last = idx + ch.len_utf8();
    }
    last.max(s.chars().next().map(char::len_utf8).unwrap_or(0))
}

fn split_at_last_space(s: &str) -> Option<usize> {
    let idx = s
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| ch.is_whitespace().then_some(idx))?;
    (idx > 0).then_some(idx)
}

/// Visible width of the timestamp/level/scope prefix up to where the message
/// starts, used to indent continuation lines of multi-line messages so they
/// align under the first line's message column.
fn message_column_width(scope_visible_width: usize) -> usize {
    // "HH:MM:SS" (8) + " " + level right-aligned to 5 + " " + scope + " "
    8 + 1 + 5 + 1 + scope_visible_width + 1
}

fn level_color(level: &LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "\x1b[38;2;140;207;255m",
        LogLevel::Info => "\x1b[38;2;155;217;179m",
        LogLevel::Warn => "\x1b[38;2;234;211;156m",
        LogLevel::Error => "\x1b[38;2;232;163;160m",
        LogLevel::Fatal => "\x1b[38;2;200;166;242m",
    }
}
