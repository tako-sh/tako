mod json;
mod remote;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::commands::project_context;
use crate::commands::server;
use crate::config::{ServersToml, TakoToml};
use crate::output;
use json::JsonLogWriter;
use json::format_json_lines;
use remote::stream_remote_logs;
use remote::{build_fetch_log_command, build_tail_log_command, collect_remote_log_bytes};
use tracing::Instrument;

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

#[derive(Clone, Copy)]
struct LogOutputOptions {
    show_prefix: bool,
    colorize: bool,
    json: bool,
}

pub fn run(
    requested_env: Option<&str>,
    tail: bool,
    days: u32,
    json: bool,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(requested_env, tail, days, json, config_path))
}

async fn run_async(
    requested_env: Option<&str>,
    tail: bool,
    days: u32,
    json: bool,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let context = project_context::resolve_existing(config_path)?;

    let tako_config = TakoToml::load_from_file(&context.config_path)?;
    let mut servers = ServersToml::load()?;

    let env = super::helpers::resolve_env(requested_env);

    let app_name = crate::app::require_app_name_from_config_path(&context.config_path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    let remote_app_name = tako_core::deployment_app_id(&app_name, &env);

    if !tako_config.envs.contains_key(env.as_str()) {
        let available: Vec<_> = tako_config.envs.keys().collect();
        return Err(format!(
            "Environment '{}' not found. Available: {}",
            env,
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        )
        .into());
    }

    let server_names = resolve_log_server_names(&tako_config, &mut servers, &env).await?;

    let colorize = output::is_interactive();
    let show_prefix = server_names.len() > 1;
    let log_output = LogOutputOptions {
        show_prefix,
        colorize,
        json,
    };
    let route_filters = tako_config.get_routes(&env).unwrap_or_default();

    if tail {
        if requested_env.is_some() && !json {
            output::warning(&format!("Using {} environment", output::accent(&env)));
        }
        stream_logs(
            &server_names,
            &servers,
            &remote_app_name,
            &route_filters,
            show_prefix,
            colorize,
            json,
        )
        .await
    } else {
        if requested_env.is_some() && !json {
            output::warning(&format!("Using {} environment", output::accent(&env)));
        }
        if !json {
            output::hint(&format!(
                "Showing logs for the last {days} days. Use {} to change",
                output::strong("--days")
            ));
        }
        fetch_logs(
            &server_names,
            &servers,
            &remote_app_name,
            &route_filters,
            days,
            log_output,
        )
        .await
    }
}

async fn stream_logs(
    server_names: &[String],
    servers: &ServersToml,
    app_name: &str,
    route_filters: &[String],
    show_prefix: bool,
    colorize: bool,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !json {
        output::info(&format!(
            "Streaming logs for {} {}",
            output::strong(app_name),
            output::theme_muted("(Ctrl+c to stop)")
        ));
    }

    let writer: Arc<Mutex<Box<dyn Write + Send>>> =
        Arc::new(Mutex::new(Box::new(std::io::stdout())));

    let mut tasks = Vec::new();
    for server_name in server_names {
        let server = servers
            .get(server_name)
            .ok_or_else(|| server_not_found_error(server_name))?;

        let host = server.host.clone();
        let port = server.port;
        let app_name = app_name.to_string();
        let route_filters = route_filters.to_vec();
        let writer = writer.clone();
        let prefix = format_prefix(server_name, show_prefix, colorize);
        let name = server_name.to_string();
        let span = output::scope(&name);

        tasks.push(tokio::spawn(
            async move {
                let _t = output::timed(&format!("Stream logs ({host}:{port})"));
                let log_cmd = build_tail_log_command(&app_name, &route_filters);

                if json {
                    let lw = Arc::new(Mutex::new(JsonLogWriter::new(writer, name)));
                    let sink = {
                        let lw = lw.clone();
                        Arc::new(move |data: &[u8]| {
                            if let Ok(mut w) = lw.lock() {
                                w.push(data);
                            }
                        })
                    };
                    stream_remote_logs(&host, port, &log_cmd, sink).await?;

                    if let Ok(mut w) = lw.lock() {
                        w.flush();
                    }
                } else {
                    let lw = Arc::new(Mutex::new(LogWriter::new(writer, prefix, colorize)));
                    let sink = {
                        let lw = lw.clone();
                        Arc::new(move |data: &[u8]| {
                            if let Ok(mut w) = lw.lock() {
                                w.push(data);
                            }
                        })
                    };
                    stream_remote_logs(&host, port, &log_cmd, sink).await?;

                    if let Ok(mut w) = lw.lock() {
                        w.flush();
                    }
                }
                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            }
            .instrument(span),
        ));
    }

    for t in tasks {
        match t.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(format!("Failed to stream logs: {error}").into()),
            Err(error) => return Err(format!("Failed to stream logs: {error}").into()),
        }
    }
    Ok(())
}

async fn fetch_logs(
    server_names: &[String],
    servers: &ServersToml,
    app_name: &str,
    route_filters: &[String],
    days: u32,
    output_options: LogOutputOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let total = server_names.len();
    let progress_label = if total > 1 {
        format!("Fetching logs… {}", output::muted_progress(0, total))
    } else {
        "Fetching logs…".to_string()
    };
    let phase = (!output_options.json).then(|| output::PhaseSpinner::start(&progress_label));

    let collected: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let done_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut tasks = Vec::new();
    for server_name in server_names {
        let server = servers
            .get(server_name)
            .ok_or_else(|| server_not_found_error(server_name))?;

        let host = server.host.clone();
        let port = server.port;
        let app_name = app_name.to_string();
        let route_filters = route_filters.to_vec();
        let server_name = server_name.to_string();
        let collected = collected.clone();
        let done_count = done_count.clone();
        let phase_pb = phase.as_ref().and_then(|phase| phase.pb().cloned());
        let span = output::scope(&server_name);

        tasks.push(tokio::spawn(
            async move {
                let _t = output::timed(&format!("Fetch logs ({host}:{port}, last {days} days)"));
                // Read app log files (primary) and server logs about this app (supplementary).
                // Pipe through zstd if available on the server; falls back to raw output.
                let log_cmd = build_fetch_log_command(&app_name, &route_filters, days);

                let collector = Arc::new(Mutex::new(ByteCollector::new(server_name, collected)));
                let bytes = collect_remote_log_bytes(&host, port, &log_cmd).await?;

                if let Ok(mut c) = collector.lock() {
                    c.push(&bytes);
                }

                if let Ok(c) = Arc::try_unwrap(collector) {
                    c.into_inner().unwrap().finish();
                }

                if total > 1 {
                    let done = done_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if let Some(ref pb) = phase_pb {
                        pb.set_message(format!(
                            "Fetching logs… {}",
                            output::muted_progress(done, total)
                        ));
                    }
                }

                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            }
            .instrument(span),
        ));
    }

    let mut task_errors = Vec::new();
    for t in tasks {
        match t.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => task_errors.push(error.to_string()),
            Err(error) => task_errors.push(error.to_string()),
        }
    }

    if !task_errors.is_empty() {
        return Err(format!("Failed to fetch logs:\n{}", task_errors.join("\n")).into());
    }

    // Sort by timestamp across all servers.
    let mut lines = match Arc::try_unwrap(collected) {
        Ok(m) => m.into_inner().unwrap_or_default(),
        Err(arc) => arc.lock().unwrap_or_else(|e| e.into_inner()).clone(),
    };
    lines.sort_by(|a, b| extract_timestamp(&a.1).cmp(extract_timestamp(&b.1)));

    if lines.is_empty() {
        if let Some(phase) = phase {
            phase.finish("No logs found");
        }
        if !output_options.json {
            output::warning(&format!(
                "No logs in the last {} days. Try {} to stream live logs.",
                days,
                output::strong("--tail")
            ));
        }
        return Ok(());
    }

    if let Some(phase) = phase {
        phase.finish("Logs fetched");
    }

    if output_options.json {
        print!("{}", format_json_lines(&lines));
        return Ok(());
    }

    // Format and dedup.
    let formatted = format_and_dedup(&lines, output_options.show_prefix, output_options.colorize);

    // Show in pager or print directly.
    if output::is_interactive() {
        if let Some(mut child) = spawn_pager() {
            if let Some(ref mut stdin) = child.stdin {
                let _ = stdin.write_all(formatted.as_bytes());
            }
            drop(child.stdin.take());
            let _ = child.wait();
        } else {
            print!("{formatted}");
        }
    } else {
        print!("{formatted}");
    }

    Ok(())
}

fn format_and_dedup(lines: &[(String, String)], show_prefix: bool, colorize: bool) -> String {
    let mut out = String::new();
    let mut last_key = String::new();
    let mut repeat_count: u32 = 0;

    for (server, raw) in lines {
        let (key, formatted) = format_log_entry(raw, colorize);
        if !key.is_empty() && key == last_key {
            repeat_count += 1;
        } else {
            push_repeat(&mut out, repeat_count, colorize);
            let prefix = format_prefix(server, show_prefix, colorize);
            out.push_str(&prefix);
            out.push_str(&formatted);
            out.push('\n');
            last_key = key;
            repeat_count = 0;
        }
    }
    push_repeat(&mut out, repeat_count, colorize);
    out
}

fn push_repeat(out: &mut String, count: u32, colorize: bool) {
    if count > 0 {
        // Indent to align with the message column (date + space + time + space = 20 chars).
        if colorize {
            out.push_str(&format!(
                "                    {DIM}… and {count} more{RESET}\n"
            ));
        } else {
            out.push_str(&format!("                    … and {count} more\n"));
        }
    }
}

/// Zstd magic number (first 4 bytes of any zstd frame).
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

struct ByteCollector {
    bytes: Vec<u8>,
    server: String,
    lines: Arc<Mutex<Vec<(String, String)>>>,
}

impl ByteCollector {
    fn new(server: String, lines: Arc<Mutex<Vec<(String, String)>>>) -> Self {
        Self {
            bytes: Vec::new(),
            server,
            lines,
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
    }

    fn finish(self) {
        let text = if self.bytes.len() >= 4 && self.bytes[..4] == ZSTD_MAGIC {
            match zstd::stream::decode_all(std::io::Cursor::new(&self.bytes)) {
                Ok(decompressed) => String::from_utf8_lossy(&decompressed).into_owned(),
                Err(_) => String::from_utf8_lossy(&self.bytes).into_owned(),
            }
        } else {
            String::from_utf8_lossy(&self.bytes).into_owned()
        };

        let mut lines = self.lines.lock().unwrap();
        for line in text.lines() {
            if !line.is_empty() {
                lines.push((self.server.clone(), line.to_string()));
            }
        }
    }
}

struct LogWriter {
    buf: String,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    prefix: String,
    colorize: bool,
    last_msg_key: String,
    repeat_count: u32,
}

impl LogWriter {
    fn new(writer: Arc<Mutex<Box<dyn Write + Send>>>, prefix: String, colorize: bool) -> Self {
        Self {
            buf: String::new(),
            writer,
            prefix,
            colorize,
            last_msg_key: String::new(),
            repeat_count: 0,
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.buf.push_str(&String::from_utf8_lossy(data));
        while let Some(nl) = self.buf.find('\n') {
            let line = self.buf[..nl].to_string();
            self.buf = self.buf[nl + 1..].to_string();
            self.process_line(&line);
        }
    }

    fn process_line(&mut self, line: &str) {
        let (key, formatted) = format_log_entry(line, self.colorize);
        if !key.is_empty() && key == self.last_msg_key {
            self.repeat_count += 1;
        } else {
            self.flush_repeat();
            self.write_line(&formatted);
            self.last_msg_key = key;
            self.repeat_count = 0;
        }
    }

    fn flush_repeat(&mut self) {
        if self.repeat_count > 0 {
            let msg = if self.colorize {
                format!("         {DIM}… and {} more{RESET}", self.repeat_count)
            } else {
                format!("         … and {} more", self.repeat_count)
            };
            self.write_line(&msg);
        }
    }

    fn flush(&mut self) {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.process_line(&line);
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

fn format_prefix(server: &str, show: bool, colorize: bool) -> String {
    if !show {
        return String::new();
    }
    if colorize {
        format!("{DIM}[{server}]{RESET} ")
    } else {
        format!("[{server}] ")
    }
}

fn format_log_entry(line: &str, colorize: bool) -> (String, String) {
    if let Some((hms, level, message)) = parse_json_log(line) {
        let key = format!("{level} {message}");
        let formatted = if colorize {
            let color = level_color(&level);
            format!("{DIM}{hms}{RESET} {color}{level:>5}{RESET} {message}")
        } else {
            format!("{hms} {level:>5} {message}")
        };
        (key, formatted)
    } else {
        // Non-JSON line (e.g., from app log files): show as-is.
        (String::new(), line.to_string())
    }
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

    // Collect structured fields (skip "message") into "key=value" pairs.
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

    let hms = if timestamp.len() >= 19 {
        format!("{} {}", &timestamp[..10], &timestamp[11..19])
    } else {
        timestamp.to_string()
    };

    Some((hms, level.to_string(), parts.join(" ")))
}

fn extract_timestamp(line: &str) -> &str {
    // JSON: look for "timestamp":"..." field.
    if let Some(pos) = line.find("\"timestamp\":\"") {
        let start = pos + 13;
        if let Some(end) = line[start..].find('"') {
            return &line[start..start + end];
        }
    }
    // App log format: "2026-04-03T12:00:00.000Z [out] [inst-1] ..."
    if line.len() >= 24 && line.as_bytes()[4] == b'-' && line.as_bytes()[10] == b'T' {
        return &line[..24];
    }
    "\x7f" // sort unparseable lines last
}

fn level_color(level: &str) -> &'static str {
    match level {
        "DEBUG" | "TRACE" => "\x1b[38;2;140;207;255m",
        "INFO" => "\x1b[38;2;155;217;179m",
        "WARN" => "\x1b[38;2;234;211;156m",
        "ERROR" => "\x1b[38;2;232;163;160m",
        _ => "",
    }
}

fn spawn_pager() -> Option<std::process::Child> {
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less -R".to_string());
    let parts: Vec<&str> = pager.split_whitespace().collect();
    let (cmd, args) = parts.split_first()?;
    Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .ok()
}

fn server_not_found_error(name: &str) -> Box<dyn std::error::Error> {
    format!(
        "Server '{}' not found in config.toml [[servers]]. Run 'tako servers add --name {} <host>'.",
        name, name
    )
    .into()
}

async fn resolve_log_server_names(
    tako_config: &TakoToml,
    servers: &mut ServersToml,
    env: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    match super::helpers::resolve_servers_for_env(tako_config, servers, env) {
        Ok(mut resolved) => {
            resolved.sort();
            resolved.dedup();
            super::helpers::validate_server_names(&resolved, servers)
                .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
            Ok(resolved)
        }
        Err(_) if env == "production" && servers.is_empty() => {
            if server::prompt_to_add_server(
                "No servers have been added. Logs need at least one server.",
            )
            .await?
            .is_some()
            {
                *servers = ServersToml::load()?;
                if servers.len() == 1 {
                    let only = servers.names()[0];
                    return Ok(vec![only.to_string()]);
                }
            }
            Err(
                "No servers have been added. Run 'tako servers add <host>' first, \
                 then add it under [envs.production].servers in tako.toml."
                    .into(),
            )
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerEntry;

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
        let output = format_and_dedup(&lines, false, false);
        let result: Vec<&str> = output.trim().lines().collect();
        assert_eq!(result.len(), 3);
        assert!(result[0].contains("hello"));
        assert!(result[1].contains("… and 2 more"));
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
        let a = r#"{"timestamp":"2026-03-10T12:00:02.000Z","level":"INFO","fields":{"message":"second"}}"#;
        let b = r#"{"timestamp":"2026-03-10T12:00:01.000Z","level":"INFO","fields":{"message":"first"}}"#;
        assert!(extract_timestamp(b) < extract_timestamp(a));
    }

    #[tokio::test]
    async fn resolve_log_server_names_uses_explicit_env_mapping() {
        let tako_config = TakoToml::parse(
            r#"
[envs.production]
route = "app.example.com"
servers = ["solo"]
"#,
        )
        .unwrap();
        let mut servers = ServersToml::default();
        servers.servers.insert(
            "solo".to_string(),
            ServerEntry {
                host: "127.0.0.1".to_string(),
                port: 22,
                description: None,
            },
        );

        let names = resolve_log_server_names(&tako_config, &mut servers, "production")
            .await
            .expect("should resolve explicit mapping");
        assert_eq!(names, vec!["solo".to_string()]);
    }

    #[tokio::test]
    async fn resolve_log_server_names_errors_for_non_production_without_mapping() {
        let tako_config = TakoToml::default();
        let mut servers = ServersToml::default();
        servers.servers.insert(
            "solo".to_string(),
            ServerEntry {
                host: "127.0.0.1".to_string(),
                port: 22,
                description: None,
            },
        );

        let err = resolve_log_server_names(&tako_config, &mut servers, "staging")
            .await
            .expect_err("should fail for non-production");
        assert!(
            err.to_string()
                .contains("No servers configured for environment 'staging'")
        );
    }

    #[test]
    fn byte_collector_decompresses_zstd() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let mut collector = ByteCollector::new("s1".to_string(), lines.clone());

        let raw = b"line one\nline two\nline three\n";
        let compressed = zstd::bulk::compress(raw, 3).unwrap();
        collector.push(&compressed);
        collector.finish();

        let result = lines.lock().unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], ("s1".to_string(), "line one".to_string()));
        assert_eq!(result[1], ("s1".to_string(), "line two".to_string()));
        assert_eq!(result[2], ("s1".to_string(), "line three".to_string()));
    }

    #[test]
    fn byte_collector_handles_raw_text() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let mut collector = ByteCollector::new("s1".to_string(), lines.clone());

        collector.push(b"hello\nworld\n");
        collector.finish();

        let result = lines.lock().unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].1, "hello");
        assert_eq!(result[1].1, "world");
    }
}
