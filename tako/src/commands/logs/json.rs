use std::io::Write;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

pub(super) fn format_json_lines(lines: &[(String, String)]) -> String {
    let mut out = String::new();
    for (server, raw) in lines {
        out.push_str(&format_json_line(server, raw));
        out.push('\n');
    }
    out
}

pub(super) struct JsonLogWriter {
    buf: String,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    server: String,
}

impl JsonLogWriter {
    pub(super) fn new(writer: Arc<Mutex<Box<dyn Write + Send>>>, server: String) -> Self {
        Self {
            buf: String::new(),
            writer,
            server,
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
        let _ = writeln!(writer, "{}", format_json_line(&self.server, line));
    }
}

fn format_json_line(server: &str, raw: &str) -> String {
    let record = json_record(server, raw);
    serde_json::to_string(&record).unwrap_or_else(|_| "{}".to_string())
}

fn json_record(server: &str, raw: &str) -> Value {
    if let Some(record) = tracing_json_record(server, raw) {
        return record;
    }
    if let Some(record) = app_log_record(server, raw) {
        return record;
    }

    compact_object(vec![
        ("srv", json!(server)),
        ("src", json!("unknown")),
        ("msg", json!(raw)),
    ])
}

fn tracing_json_record(server: &str, raw: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let ts = value.get("timestamp").and_then(Value::as_str)?;
    let lvl = value.get("level").and_then(Value::as_str)?;
    let fields = value.get("fields").and_then(Value::as_object)?;
    let msg = fields
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let mut out = Map::new();
    insert_str(&mut out, "ts", ts);
    insert_str(&mut out, "lvl", &lvl.to_ascii_lowercase());
    insert_str(&mut out, "srv", server);
    insert_str(&mut out, "src", infer_source(fields, msg));
    insert_str(&mut out, "event", infer_event(msg));
    copy_str_field(fields, &mut out, "app", "app");
    copy_str_field(fields, &mut out, "instance", "inst");
    copy_str_field(fields, &mut out, "new_instance", "new_inst");
    copy_str_field(fields, &mut out, "old_instance", "old_inst");
    copy_field(fields, &mut out, "failures", "failures");
    copy_str_field(fields, &mut out, "reason", "reason");

    if let Some(status) = extract_status(msg) {
        out.insert("status".to_string(), json!(status));
    }
    if let Some(route) = extract_route(msg) {
        insert_str(&mut out, "route", &route);
    }
    if let Some(upstream) = extract_upstream(msg) {
        insert_str(&mut out, "upstream", &upstream);
    }
    if let Some(err) = extract_error(msg) {
        insert_str(&mut out, "err", &err);
    }
    insert_str(&mut out, "msg", msg);

    Some(Value::Object(out))
}

fn app_log_record(server: &str, raw: &str) -> Option<Value> {
    let (ts, rest) = raw.split_once(' ')?;
    if ts.len() < 20 || !ts.contains('T') {
        return None;
    }
    let (stream, rest) = bracketed(rest)?;
    let (inst, msg) = bracketed(rest.trim_start())?;

    let mut out = Map::new();
    insert_str(&mut out, "ts", ts);
    insert_str(&mut out, "srv", server);
    insert_str(&mut out, "src", "app");
    insert_str(&mut out, "stream", stream);
    insert_str(&mut out, "inst", inst);

    if let Ok(inner) = serde_json::from_str::<Value>(msg) {
        if let Some(level) = inner.get("level").and_then(Value::as_str) {
            insert_str(&mut out, "lvl", level);
        }
        if let Some(scope) = inner.get("scope").and_then(Value::as_str) {
            insert_str(&mut out, "scope", scope);
        }
        if let Some(inner_msg) = inner.get("msg").and_then(Value::as_str) {
            insert_str(&mut out, "msg", inner_msg);
        } else {
            insert_str(&mut out, "msg", msg);
        }
        if let Some(code) = inner
            .get("fields")
            .and_then(Value::as_object)
            .and_then(|fields| fields.get("code"))
            .and_then(Value::as_str)
        {
            insert_str(&mut out, "code", code);
        }
    } else {
        insert_str(&mut out, "msg", msg);
    }

    Some(Value::Object(out))
}

fn bracketed(input: &str) -> Option<(&str, &str)> {
    let rest = input.strip_prefix('[')?;
    let (value, rest) = rest.split_once(']')?;
    Some((value, rest.trim_start()))
}

fn compact_object(items: Vec<(&str, Value)>) -> Value {
    let mut out = Map::new();
    for (key, value) in items {
        if !value.is_null() {
            out.insert(key.to_string(), value);
        }
    }
    Value::Object(out)
}

fn insert_str(out: &mut Map<String, Value>, key: &str, value: &str) {
    if !value.is_empty() {
        out.insert(key.to_string(), json!(value));
    }
}

fn copy_str_field(fields: &Map<String, Value>, out: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(value) = fields.get(from).and_then(Value::as_str) {
        insert_str(out, to, value);
    }
}

fn copy_field(fields: &Map<String, Value>, out: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(value) = fields.get(from)
        && (value.is_string() || value.is_number() || value.is_boolean())
    {
        out.insert(to.to_string(), value.clone());
    }
}

fn infer_source(fields: &Map<String, Value>, msg: &str) -> &'static str {
    if msg.contains("Fail to proxy")
        || fields
            .get("log.target")
            .and_then(Value::as_str)
            .is_some_and(|target| target.contains("pingora"))
    {
        "proxy"
    } else {
        "server"
    }
}

fn infer_event(msg: &str) -> &'static str {
    if msg.contains("Fail to proxy") {
        "proxy_failed"
    } else if msg.contains("Instance marked dead") || msg.contains("Instance is dead") {
        "instance_dead"
    } else if msg.contains("Replacing dead instance") {
        "replacement_started"
    } else if msg.contains("Successfully spawned replacement instance") {
        "replacement_healthy"
    } else if msg.contains("Failed to spawn replacement instance") {
        "replacement_failed"
    } else if msg.contains("Spawning instance") {
        "instance_spawning"
    } else if msg.contains("Instance process exited") {
        "instance_exited"
    } else if msg.contains("Instance is healthy") {
        "instance_healthy"
    } else if msg.contains("Instance ready") {
        "instance_ready"
    } else if msg.contains("Restored and started app") {
        "app_restored"
    } else {
        "log"
    }
}

fn extract_status(msg: &str) -> Option<u16> {
    extract_after(msg, "status: ")
        .and_then(|value| value.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|value| value.parse().ok())
}

fn extract_route(msg: &str) -> Option<String> {
    extract_after(msg, "Host: ").map(|value| {
        value
            .split(|c: char| c == ',' || c.is_whitespace())
            .next()
            .unwrap_or(value)
            .trim_end_matches(':')
            .to_string()
    })
}

fn extract_upstream(msg: &str) -> Option<String> {
    extract_after(msg, "addr: ")
        .or_else(|| extract_after(msg, "Fail to connect to "))
        .map(|value| {
            value
                .split(|c: char| c == ',' || c.is_whitespace())
                .next()
                .unwrap_or(value)
                .to_string()
        })
}

fn extract_error(msg: &str) -> Option<String> {
    if msg.contains("Connection refused") {
        Some("Connection refused".to_string())
    } else if msg.contains("ConnectionClosed") {
        Some("ConnectionClosed".to_string())
    } else if msg.contains("ConnectRefused") {
        Some("ConnectRefused".to_string())
    } else {
        None
    }
}

fn extract_after<'a>(msg: &'a str, needle: &str) -> Option<&'a str> {
    let start = msg.find(needle)? + needle.len();
    Some(&msg[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_proxy_failure_as_compact_json() {
        let raw = r#"{"timestamp":"2026-05-03T15:08:19.000Z","level":"ERROR","fields":{"message":"Fail to proxy: Upstream ConnectRefused context: Fail to connect to addr: 127.0.0.1:39063, scheme: HTTP cause: Connection refused (os error 111), status: 502, tries: 2, retry: false, GET /, Host: demo.tako.sh:","log.target":"pingora_proxy"}}"#;
        let line = format_json_line("prod", raw);
        let value: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(value["ts"], "2026-05-03T15:08:19.000Z");
        assert_eq!(value["lvl"], "error");
        assert_eq!(value["srv"], "prod");
        assert_eq!(value["src"], "proxy");
        assert_eq!(value["event"], "proxy_failed");
        assert_eq!(value["route"], "demo.tako.sh");
        assert_eq!(value["status"], 502);
        assert_eq!(value["upstream"], "127.0.0.1:39063");
        assert_eq!(value["err"], "Connection refused");
    }

    #[test]
    fn formats_instance_health_as_compact_json() {
        let raw = r#"{"timestamp":"2026-05-04T07:03:55.000Z","level":"INFO","fields":{"message":"Instance is healthy","app":"demo/production","instance":"ieTkoBFI"}}"#;
        let line = format_json_line("prod", raw);
        let value: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(value["src"], "server");
        assert_eq!(value["event"], "instance_healthy");
        assert_eq!(value["app"], "demo/production");
        assert_eq!(value["inst"], "ieTkoBFI");
        assert!(value.get("route").is_none());
    }

    #[test]
    fn formats_app_log_line_as_compact_json() {
        let raw = r#"2026-04-20T06:19:53.716Z [out] [vdACgHcC] {"level":"error","scope":"sdk.rpc","msg":"rpc rejected","fields":{"code":"TAKO_RPC_ERROR"}}"#;
        let line = format_json_line("prod", raw);
        let value: Value = serde_json::from_str(&line).unwrap();

        assert_eq!(value["src"], "app");
        assert_eq!(value["stream"], "out");
        assert_eq!(value["inst"], "vdACgHcC");
        assert_eq!(value["lvl"], "error");
        assert_eq!(value["scope"], "sdk.rpc");
        assert_eq!(value["msg"], "rpc rejected");
        assert_eq!(value["code"], "TAKO_RPC_ERROR");
    }

    #[test]
    fn json_lines_are_one_record_per_line() {
        let lines = vec![
            ("prod".to_string(), "raw one".to_string()),
            ("prod".to_string(), "raw two".to_string()),
        ];
        let output = format_json_lines(&lines);
        let records: Vec<&str> = output.lines().collect();

        assert_eq!(records.len(), 2);
        assert!(serde_json::from_str::<Value>(records[0]).is_ok());
        assert!(serde_json::from_str::<Value>(records[1]).is_ok());
    }
}
