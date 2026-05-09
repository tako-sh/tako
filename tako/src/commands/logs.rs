mod json;
mod remote;
mod render;

use std::io::{IsTerminal, Write};
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
use render::{LogWriter, extract_timestamp, format_and_dedup, format_prefix};
use tracing::Instrument;

#[derive(Clone)]
struct LogOutputOptions {
    show_prefix: bool,
    colorize: bool,
    page: bool,
    json: bool,
    app_name: String,
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
    if let Some(runtime) = &tako_config.runtime {
        super::log_style::set_app_runtime(runtime.clone());
    }
    let mut servers = ServersToml::load()?;

    let env = super::helpers::resolve_env_silent(requested_env);

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

    let interactive_stdout = logs_stdout_is_interactive();
    let colorize = interactive_stdout;
    let show_prefix = server_names.len() > 1;
    let log_output = LogOutputOptions {
        show_prefix,
        colorize,
        page: interactive_stdout,
        json,
        app_name: app_name.clone(),
    };

    if tail {
        if !json {
            output::hint(&super::helpers::format_environment_notice(&env));
        }
        stream_logs(
            &server_names,
            &servers,
            &remote_app_name,
            &app_name,
            show_prefix,
            colorize,
            json,
        )
        .await
    } else {
        if !json {
            output::hint(&super::helpers::format_environment_notice(&env));
        }
        if !json {
            output::hint(&format!(
                "Showing logs for the last {days} days. Use {} to change",
                output::strong("--days")
            ));
        }
        fetch_logs(&server_names, &servers, &remote_app_name, days, log_output).await
    }
}

async fn stream_logs(
    server_names: &[String],
    servers: &ServersToml,
    remote_app_name: &str,
    display_app_name: &str,
    show_prefix: bool,
    colorize: bool,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !json {
        output::info(&format!(
            "Streaming logs for {} {}",
            output::strong(display_app_name),
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
        let remote_app_name = remote_app_name.to_string();
        let display_app_name = display_app_name.to_string();
        let writer = writer.clone();
        let prefix = format_prefix(server_name, show_prefix, colorize);
        let name = server_name.to_string();
        let span = output::scope(&name);

        tasks.push(tokio::spawn(
            async move {
                let _t = output::timed(&format!("Stream logs ({host}:{port})"));
                let log_cmd = build_tail_log_command(&remote_app_name);

                if json {
                    let lw = Arc::new(Mutex::new(JsonLogWriter::new(writer, name, show_prefix)));
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
                    let lw = Arc::new(Mutex::new(LogWriter::new(
                        writer,
                        prefix,
                        display_app_name,
                        colorize,
                    )));
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
        let server_name = server_name.to_string();
        let collected = collected.clone();
        let done_count = done_count.clone();
        let phase_pb = phase.as_ref().and_then(|phase| phase.pb().cloned());
        let span = output::scope(&server_name);

        tasks.push(tokio::spawn(
            async move {
                let _t = output::timed(&format!("Fetch logs ({host}:{port}, last {days} days)"));
                // Read app log files and pipe through zstd if available on the server.
                let log_cmd = build_fetch_log_command(&app_name, days);

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
        print!("{}", format_json_lines(&lines, output_options.show_prefix));
        return Ok(());
    }

    // Format and dedup.
    let formatted = format_and_dedup(
        &lines,
        &output_options.app_name,
        output_options.show_prefix,
        output_options.colorize,
    );

    // Show history in a pager for consistent search/navigation in interactive terminals.
    if output_options.page {
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

fn logs_stdout_is_interactive() -> bool {
    should_page_logs(output::is_interactive(), std::io::stdout().is_terminal())
}

fn should_page_logs(interactive: bool, stdout_is_terminal: bool) -> bool {
    interactive && stdout_is_terminal
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

#[derive(Debug, PartialEq, Eq)]
struct PagerCommand {
    program: String,
    args: Vec<String>,
    less_env: Option<String>,
}

impl PagerCommand {
    fn new(program: impl Into<String>, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            less_env: None,
        }
    }

    fn default_less() -> Self {
        Self {
            program: "less".to_string(),
            args: vec!["-R".to_string()],
            less_env: Some("-R".to_string()),
        }
    }

    fn shell_with_raw_ansi(pager: &str) -> Self {
        Self {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), pager.to_string()],
            less_env: Some("-R".to_string()),
        }
    }
}

fn pager_command(pager: Option<&str>) -> PagerCommand {
    match pager.map(str::trim).filter(|pager| !pager.is_empty()) {
        Some(pager) if is_diff_only_pager(pager) => PagerCommand::default_less(),
        Some(pager) if is_less_pager(pager) => PagerCommand::shell_with_raw_ansi(pager),
        Some(pager) => PagerCommand::new("sh", ["-c", pager]),
        None => PagerCommand::default_less(),
    }
}

fn spawn_pager() -> Option<std::process::Child> {
    let pager = std::env::var("PAGER").ok();
    spawn_pager_command(&pager_command(pager.as_deref()))
        .or_else(|| spawn_pager_command(&PagerCommand::new("more", std::iter::empty::<&str>())))
}

fn spawn_pager_command(pager: &PagerCommand) -> Option<std::process::Child> {
    let mut command = Command::new(&pager.program);
    command.args(&pager.args).stdin(Stdio::piped());
    if let Some(less_env) = &pager.less_env {
        command.env("LESS", less_env);
    }
    command.spawn().ok()
}

fn is_diff_only_pager(pager: &str) -> bool {
    pager_program_names(pager).any(|name| matches!(name, "delta" | "git-delta"))
}

fn is_less_pager(pager: &str) -> bool {
    pager_program_names(pager).any(|name| name == "less")
}

fn pager_program_names(pager: &str) -> impl Iterator<Item = &str> {
    pager.split_whitespace().filter_map(|token| {
        if token.contains('=') {
            return None;
        }
        std::path::Path::new(token)
            .file_name()
            .and_then(|name| name.to_str())
    })
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

    #[test]
    fn default_pager_preserves_ansi_colors() {
        assert_eq!(pager_command(None), PagerCommand::default_less());
        assert_eq!(pager_command(Some("   ")), PagerCommand::default_less());
    }

    #[test]
    fn default_pager_ignores_less_quit_if_one_screen() {
        let pager = pager_command(None);
        assert_eq!(pager.less_env.as_deref(), Some("-R"));
    }

    #[test]
    fn custom_pager_runs_as_shell_command() {
        assert_eq!(
            pager_command(Some("most -w")),
            PagerCommand::new("sh", ["-c", "most -w"])
        );
    }

    #[test]
    fn less_pager_preserves_args_and_raw_ansi() {
        assert_eq!(
            pager_command(Some("less -S --pattern 'ERROR 5WHq7f05'")),
            PagerCommand::shell_with_raw_ansi("less -S --pattern 'ERROR 5WHq7f05'")
        );
    }

    #[test]
    fn diff_only_pager_falls_back_to_less() {
        assert_eq!(pager_command(Some("delta")), PagerCommand::default_less());
        assert_eq!(
            pager_command(Some("/opt/homebrew/bin/delta --dark")),
            PagerCommand::default_less()
        );
        assert_eq!(
            pager_command(Some("env DELTA_FEATURES=side-by-side delta")),
            PagerCommand::default_less()
        );
    }

    #[test]
    fn pager_requires_interactive_stdout() {
        assert!(should_page_logs(true, true));
        assert!(!should_page_logs(true, false));
        assert!(!should_page_logs(false, true));
    }
}
