use std::io::Write;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

pub(super) fn format_json_lines(lines: &[(String, String)], include_server: bool) -> String {
    let mut out = String::new();
    for (server, raw) in lines {
        out.push_str(&format_json_line(server, raw, include_server));
        out.push('\n');
    }
    out
}

pub(super) struct JsonLogWriter {
    buf: String,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    server: String,
    include_server: bool,
}

impl JsonLogWriter {
    pub(super) fn new(
        writer: Arc<Mutex<Box<dyn Write + Send>>>,
        server: String,
        include_server: bool,
    ) -> Self {
        Self {
            buf: String::new(),
            writer,
            server,
            include_server,
        }
    }

    pub(super) fn push(&mut self, data: &[u8]) {
        self.buf.push_str(&String::from_utf8_lossy(data));
        while let Some(nl) = self.buf.find('\n') {
            let line = self.buf[..nl].to_string();
            self.buf = self.buf[nl + 1..].to_string();
            self.write_record(&line);
        }
    }

    pub(super) fn flush(&mut self) {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.write_record(&line);
        }
    }

    fn write_record(&self, line: &str) {
        let Ok(mut writer) = self.writer.lock() else {
            return;
        };
        let _ = writeln!(
            writer,
            "{}",
            format_json_line(&self.server, line, self.include_server)
        );
    }
}

fn format_json_line(server: &str, raw: &str, include_server: bool) -> String {
    let record = json_record(server, raw, include_server);
    serde_json::to_string(&record).unwrap_or_else(|_| "{}".to_string())
}

fn json_record(server: &str, raw: &str, include_server: bool) -> Value {
    if let Some(record) = app_log_record(server, raw, include_server) {
        return record;
    }
    if let Some(record) = structured_json_record(server, raw, include_server) {
        return record;
    }

    wrapped_text_record(server, include_server, "unknown", raw)
}

fn structured_json_record(server: &str, raw: &str, include_server: bool) -> Option<Value> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let mut out = value.as_object()?.clone();
    insert_server(&mut out, server, include_server);
    Some(Value::Object(out))
}

fn app_log_record(server: &str, raw: &str, include_server: bool) -> Option<Value> {
    let (ts, rest) = raw.split_once(' ')?;
    if ts.len() < 20 || !ts.contains('T') {
        return None;
    }
    let (stream, rest) = bracketed(rest)?;
    let (inst, msg) = bracketed(strip_log_separator(rest))?;
    let msg = strip_log_separator(msg);

    if stream == "server" && inst == "tako-server" {
        return Some(tako_record(server, include_server, ts, msg));
    }

    if let Some(record) = structured_app_record(server, include_server, inst, msg) {
        return Some(record);
    }

    Some(raw_process_record(
        server,
        include_server,
        ts,
        stream,
        inst,
        msg,
    ))
}

fn structured_app_record(
    server: &str,
    include_server: bool,
    instance_id: &str,
    msg: &str,
) -> Option<Value> {
    let value: Value = serde_json::from_str(msg).ok()?;
    let mut out = value.as_object()?.clone();
    if !is_sdk_log_record(&out) {
        return None;
    }

    let source = log_source(instance_id, &out);
    insert_str(&mut out, "source", &source);
    insert_server(&mut out, server, include_server);
    insert_str(&mut out, "instance_id", instance_id);
    Some(Value::Object(out))
}

fn is_sdk_log_record(record: &Map<String, Value>) -> bool {
    record.get("level").and_then(Value::as_str).is_some()
        && record.get("msg").and_then(Value::as_str).is_some()
}

fn raw_process_record(
    server: &str,
    include_server: bool,
    ts: &str,
    stream: &str,
    instance_id: &str,
    msg: &str,
) -> Value {
    let mut out = Map::new();
    insert_str(&mut out, "ts", ts);
    insert_str(&mut out, "source", instance_id);
    insert_server(&mut out, server, include_server);
    insert_str(&mut out, "instance_id", instance_id);
    insert_str(&mut out, "level", raw_stream_level(stream));
    insert_str(&mut out, "msg", msg);

    Value::Object(out)
}

fn tako_record(server: &str, include_server: bool, ts: &str, msg: &str) -> Value {
    let (level, msg) = split_level_prefix(msg).unwrap_or(("info", msg));

    let mut out = Map::new();
    insert_str(&mut out, "ts", ts);
    insert_str(&mut out, "source", "tako");
    insert_server(&mut out, server, include_server);
    insert_str(&mut out, "level", level);
    insert_str(&mut out, "msg", msg);

    Value::Object(out)
}

fn split_level_prefix(msg: &str) -> Option<(&'static str, &str)> {
    let (level, rest) = msg.split_once(' ')?;
    let level = match level {
        "TRACE" => "trace",
        "DEBUG" => "debug",
        "INFO" => "info",
        "WARN" | "WARNING" => "warn",
        "ERROR" => "error",
        _ => return None,
    };

    Some((level, rest))
}

fn raw_stream_level(stream: &str) -> &'static str {
    match stream {
        "err" => "error",
        _ => "info",
    }
}

fn wrapped_text_record(server: &str, include_server: bool, source: &str, msg: &str) -> Value {
    let mut out = Map::new();
    insert_str(&mut out, "source", source);
    insert_server(&mut out, server, include_server);
    insert_str(&mut out, "msg", msg);

    Value::Object(out)
}

fn insert_server(out: &mut Map<String, Value>, server: &str, include_server: bool) {
    if include_server {
        insert_str(out, "server", server);
    }
}

fn log_source(instance_id: &str, record: &Map<String, Value>) -> String {
    worker_name(record)
        .map(format_worker_source)
        .unwrap_or_else(|| instance_id.to_string())
}

fn worker_name(record: &Map<String, Value>) -> Option<&str> {
    record
        .get("worker_name")
        .and_then(Value::as_str)
        .or_else(|| {
            record
                .get("fields")
                .and_then(Value::as_object)
                .and_then(|fields| fields.get("worker_name"))
                .and_then(Value::as_str)
        })
        .filter(|name| !name.is_empty())
}

fn format_worker_source(worker_name: &str) -> String {
    match worker_name {
        "default" => "worker".to_string(),
        name => name.to_string(),
    }
}

fn bracketed(input: &str) -> Option<(&str, &str)> {
    let rest = input.strip_prefix('[')?;
    let (value, rest) = rest.split_once(']')?;
    Some((value, rest))
}

fn strip_log_separator(rest: &str) -> &str {
    rest.strip_prefix(' ').unwrap_or(rest)
}

fn insert_str(out: &mut Map<String, Value>, key: &str, value: &str) {
    if !value.is_empty() {
        out.insert(key.to_string(), json!(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_structured_json_record_without_server_for_single_server() {
        let raw = r#"{"timestamp":"2026-05-03T15:08:19.000Z","level":"ERROR","fields":{"message":"Fail to proxy: Upstream ConnectRefused context: Fail to connect to addr: 127.0.0.1:39063, scheme: HTTP cause: Connection refused (os error 111), status: 502, tries: 2, retry: false, GET /, Host: demo.tako.sh:","log.target":"pingora_proxy"}}"#;
        let line = format_json_line("prod", raw, false);
        let value: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(value["timestamp"], "2026-05-03T15:08:19.000Z");
        assert_eq!(value["level"], "ERROR");
        assert_eq!(value["fields"]["log.target"], "pingora_proxy");
        assert!(value.get("server").is_none());
        assert!(value.get("srv").is_none());
        assert!(value.get("event").is_none());
    }

    #[test]
    fn preserves_structured_app_log_record_and_adds_source_metadata() {
        let raw = r#"2026-04-20T06:19:53.716Z [out] [vdACgHcC] {"ts":1778220178800,"level":"error","scope":"sdk.rpc","msg":"rpc rejected","fields":{"code":"TAKO_RPC_ERROR"}}"#;
        let line = format_json_line("prod", raw, false);
        let value: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(value["ts"], 1778220178800_i64);
        assert_eq!(value["level"], "error");
        assert_eq!(value["scope"], "sdk.rpc");
        assert_eq!(value["msg"], "rpc rejected");
        assert_eq!(value["fields"]["code"], "TAKO_RPC_ERROR");
        assert_eq!(value["source"], "vdACgHcC");
        assert_eq!(value["instance_id"], "vdACgHcC");
        assert!(value.get("stream").is_none());
        assert!(value.get("server").is_none());
        assert!(value.get("inst").is_none());
        assert!(value.get("src").is_none());
        assert!(value.get("lvl").is_none());
    }

    #[test]
    fn includes_server_metadata_only_when_requested() {
        let raw = r#"2026-04-20T06:19:53.716Z [out] [vdACgHcC] {"ts":1778220178800,"level":"info","scope":"app","msg":"ready"}"#;
        let line = format_json_line("tako-demo", raw, true);
        let value: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(value["server"], "tako-demo");
        assert_eq!(value["source"], "vdACgHcC");
        assert_eq!(value["instance_id"], "vdACgHcC");
        assert!(value.get("stream").is_none());
    }

    #[test]
    fn worker_name_becomes_source_label() {
        let raw = r#"2026-04-20T06:19:53.716Z [out] [w-123] {"ts":1778220178800,"level":"info","scope":"worker","msg":"sent","fields":{"worker_name":"email"}}"#;
        let line = format_json_line("prod", raw, false);
        let value: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(value["source"], "email");
        assert_eq!(value["instance_id"], "w-123");
    }

    #[test]
    fn default_worker_name_renders_as_worker_source() {
        let raw = r#"2026-04-20T06:19:53.716Z [out] [w-123] {"ts":1778220178800,"level":"info","scope":"worker","msg":"sent","worker_name":"default"}"#;
        let line = format_json_line("prod", raw, false);
        let value: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(value["source"], "worker");
        assert_eq!(value["instance_id"], "w-123");
    }

    #[test]
    fn wraps_raw_app_log_text() {
        let raw = "2026-04-20T06:19:53.716Z [err] [vdACgHcC]   status: [Getter],";
        let line = format_json_line("prod", raw, false);
        let value: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(value["ts"], "2026-04-20T06:19:53.716Z");
        assert_eq!(value["source"], "vdACgHcC");
        assert_eq!(value["instance_id"], "vdACgHcC");
        assert_eq!(value["level"], "error");
        assert_eq!(value["msg"], "  status: [Getter],");
        assert!(value.get("stream").is_none());
        assert!(value.get("server").is_none());
    }

    #[test]
    fn wraps_raw_stdout_as_info_without_stream() {
        let raw = "2026-04-20T06:19:53.716Z [out] [vdACgHcC] listening";
        let line = format_json_line("prod", raw, false);
        let value: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(value["source"], "vdACgHcC");
        assert_eq!(value["instance_id"], "vdACgHcC");
        assert_eq!(value["level"], "info");
        assert_eq!(value["msg"], "listening");
        assert!(value.get("stream").is_none());
    }

    #[test]
    fn wraps_tako_server_diagnostics() {
        let raw =
            "2026-05-08T07:26:50Z [server] [tako-server] INFO Instance ready instance=zF-c2auM";
        let line = format_json_line("prod", raw, false);
        let value: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(value["ts"], "2026-05-08T07:26:50Z");
        assert_eq!(value["source"], "tako");
        assert_eq!(value["level"], "info");
        assert_eq!(value["msg"], "Instance ready instance=zF-c2auM");
        assert!(value.get("server").is_none());
    }

    #[test]
    fn json_lines_are_one_record_per_line() {
        let lines = vec![
            ("prod".to_string(), "raw one".to_string()),
            ("prod".to_string(), "raw two".to_string()),
        ];
        let output = format_json_lines(&lines, false);
        let records: Vec<&str> = output.lines().collect();

        assert_eq!(records.len(), 2);
        assert!(serde_json::from_str::<Value>(records[0]).is_ok());
        assert!(serde_json::from_str::<Value>(records[1]).is_ok());
    }
}
