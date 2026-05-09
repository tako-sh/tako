use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::commands::log_style::{DIM, RESET};

use super::{AppLogParts, parse_app_log_parts, render_log_entry, repeat_marker};

const MAX_RAW_BLOCK_LINES: usize = 200;

pub(in crate::commands::logs) struct LogWriter {
    buf: String,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    prefix: String,
    app_name: String,
    colorize: bool,
    last_msg_key: String,
    repeat_count: u32,
    last_repeat_indent: usize,
    last_repeat_start: Option<String>,
    last_repeat_through: Option<String>,
    pending_raw_block: Option<AppLogBlock>,
}

impl LogWriter {
    pub(in crate::commands::logs) fn new(
        writer: Arc<Mutex<Box<dyn Write + Send>>>,
        prefix: String,
        app_name: String,
        colorize: bool,
    ) -> Self {
        Self {
            buf: String::new(),
            writer,
            prefix,
            app_name,
            colorize,
            last_msg_key: String::new(),
            repeat_count: 0,
            last_repeat_indent: 0,
            last_repeat_start: None,
            last_repeat_through: None,
            pending_raw_block: None,
        }
    }

    pub(in crate::commands::logs) fn push(&mut self, data: &[u8]) {
        self.buf.push_str(&String::from_utf8_lossy(data));
        while let Some(nl) = self.buf.find('\n') {
            let line = self.buf[..nl].to_string();
            self.buf = self.buf[nl + 1..].to_string();
            self.process_line(&line);
        }
    }

    fn process_line(&mut self, line: &str) {
        for line in self.coalesce_stream_line(line) {
            self.write_dedup_line(&line);
        }
    }

    fn write_dedup_line(&mut self, line: &str) {
        let entry = render_log_entry(line, &self.app_name, self.colorize);
        if !entry.key.is_empty() && entry.key == self.last_msg_key {
            self.repeat_count += 1;
            self.last_repeat_through.clone_from(&entry.repeat_timestamp);
        } else {
            self.flush_repeat();
            self.write_line(&entry.formatted);
            self.last_msg_key = entry.key;
            self.last_repeat_indent = entry.repeat_indent;
            self.last_repeat_start = entry.repeat_timestamp;
            self.last_repeat_through = None;
            self.repeat_count = 0;
        }
    }

    fn coalesce_stream_line(&mut self, line: &str) -> Vec<String> {
        let Some(parts) = parse_app_log_parts(line) else {
            return self.flush_pending_then(vec![line.to_string()]);
        };

        if let Some(block) = self.pending_raw_block.as_mut()
            && block.matches("", &parts)
            && block.is_open()
        {
            block.push(parts.message);
            if block.is_closed() {
                return vec![self.pending_raw_block.take().unwrap().into_raw_line().1];
            }
            return Vec::new();
        }

        let mut ready = self.flush_pending_then(Vec::new());
        if starts_raw_block(parts.message) {
            self.pending_raw_block = Some(AppLogBlock::new("", &parts));
        } else {
            ready.push(line.to_string());
        }
        ready
    }

    fn flush_pending_then(&mut self, mut lines: Vec<String>) -> Vec<String> {
        if let Some(block) = self.pending_raw_block.take() {
            lines.insert(0, block.into_raw_line().1);
        }
        lines
    }

    fn flush_repeat(&mut self) {
        if self.repeat_count > 0 {
            let indent = " ".repeat(self.last_repeat_indent);
            let marker = repeat_marker(
                self.repeat_count,
                self.last_repeat_start.as_deref(),
                self.last_repeat_through.as_deref(),
            );
            let msg = if self.colorize {
                format!("{indent}{DIM}{marker}{RESET}")
            } else {
                format!("{indent}{marker}")
            };
            self.write_line(&msg);
        }
    }

    pub(in crate::commands::logs) fn flush(&mut self) {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.process_line(&line);
        }
        if let Some(block) = self.pending_raw_block.take() {
            self.write_dedup_line(&block.into_raw_line().1);
        }
        self.flush_repeat();
    }

    fn write_line(&self, formatted: &str) {
        let Ok(mut w) = self.writer.lock() else {
            return;
        };
        let _ = writeln!(w, "{}{formatted}", self.prefix);
    }
}

pub(super) fn coalesce_raw_app_blocks(lines: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(lines.len());
    let mut pending: Option<AppLogBlock> = None;

    for (server, raw) in lines {
        let Some(parts) = parse_app_log_parts(raw) else {
            flush_raw_block(&mut out, &mut pending);
            out.push((server.clone(), raw.clone()));
            continue;
        };

        if let Some(block) = pending.as_mut()
            && block.matches(server, &parts)
            && block.is_open()
        {
            block.push(parts.message);
            if block.is_closed() {
                flush_raw_block(&mut out, &mut pending);
            }
            continue;
        }

        flush_raw_block(&mut out, &mut pending);
        if starts_raw_block(parts.message) {
            pending = Some(AppLogBlock::new(server, &parts));
        } else {
            out.push((server.clone(), raw.clone()));
        }
    }

    flush_raw_block(&mut out, &mut pending);
    out
}

fn flush_raw_block(out: &mut Vec<(String, String)>, pending: &mut Option<AppLogBlock>) {
    if let Some(block) = pending.take() {
        out.push(block.into_raw_line());
    }
}

struct AppLogBlock {
    server: String,
    timestamp: String,
    stream: String,
    instance: String,
    lines: Vec<String>,
}

impl AppLogBlock {
    fn new(server: &str, parts: &AppLogParts<'_>) -> Self {
        Self {
            server: server.to_string(),
            timestamp: parts.timestamp.to_string(),
            stream: parts.stream.to_string(),
            instance: parts.instance.to_string(),
            lines: vec![parts.message.to_string()],
        }
    }

    fn matches(&self, server: &str, parts: &AppLogParts<'_>) -> bool {
        self.server == server && self.stream == parts.stream && self.instance == parts.instance
    }

    fn push(&mut self, message: &str) {
        if self.lines.len() < MAX_RAW_BLOCK_LINES {
            self.lines.push(message.to_string());
        }
    }

    fn is_open(&self) -> bool {
        !self.is_closed() && self.lines.len() < MAX_RAW_BLOCK_LINES
    }

    fn is_closed(&self) -> bool {
        self.lines.last().is_some_and(|line| closes_raw_block(line))
    }

    fn into_raw_line(self) -> (String, String) {
        (
            self.server,
            format!(
                "{} [{}] [{}] {}",
                self.timestamp,
                self.stream,
                self.instance,
                self.lines.join("\n")
            ),
        )
    }
}

fn starts_raw_block(message: &str) -> bool {
    message.trim_end().ends_with('{')
}

fn closes_raw_block(message: &str) -> bool {
    let trimmed = message.trim();
    trimmed.contains('}')
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '}' | ']' | ')' | '\'' | '"' | '`' | ',' | ';'))
}
