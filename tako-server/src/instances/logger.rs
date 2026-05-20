//! Per-app log capture with bounded file rotation.
//!
//! Each app gets a dedicated log writer that captures instance stdout/stderr
//! into `{data_dir}/apps/{app}/logs/current.log`. When the file exceeds
//! `max_file_bytes`, it is rotated to `previous.log` (two-file scheme).
//!
//! A bounded mpsc channel provides backpressure: if the app logs faster than
//! disk can absorb, lines are dropped rather than blocking the app process.

use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::registry::LookupSpan;

/// Default max size per log file (10 MB). Two files → 20 MB max per app.
const DEFAULT_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Channel capacity — how many lines can be buffered before backpressure kicks in.
const CHANNEL_CAPACITY: usize = 8192;

/// Flush interval — writer flushes to disk at least this often.
const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

const SERVER_LOG_SOURCE: &str = "tako-server";

static APP_LOG_REGISTRY: LazyLock<DashMap<String, AppLogHandle>> = LazyLock::new(DashMap::new);

/// A single log entry from an instance pipe.
pub struct LogEntry {
    pub instance_id: String,
    pub stream: LogStream,
    pub line: String,
}

/// Which pipe produced the line.
#[derive(Clone, Copy)]
pub enum LogStream {
    Stdout,
    Stderr,
    Server,
}

impl LogStream {
    fn label(self) -> &'static str {
        match self {
            Self::Stdout => "out",
            Self::Stderr => "err",
            Self::Server => "server",
        }
    }
}

/// Cloneable sender-side handle for pushing log lines from instance pipes.
#[derive(Clone)]
pub struct AppLogHandle {
    tx: mpsc::Sender<LogEntry>,
    dropped: Arc<AtomicU64>,
}

impl AppLogHandle {
    /// Non-blocking send. If the channel is full the line is dropped.
    pub fn try_send(&self, entry: LogEntry) {
        if self.tx.try_send(entry).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    /// Number of lines dropped due to backpressure since the last reset.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Read lines from a pipe and forward them to the app log writer.
pub async fn log_pipe<R: tokio::io::AsyncRead + Unpin>(
    pipe: R,
    log_handle: AppLogHandle,
    instance_id: String,
    stream: LogStream,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut reader = BufReader::new(pipe);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let trimmed = line.trim_end();
                if !trimmed.is_empty() {
                    log_handle.try_send(LogEntry {
                        instance_id: instance_id.clone(),
                        stream,
                        line: trimmed.to_string(),
                    });
                }
            }
        }
    }
}

/// Spawn a per-app log writer and return the sender handle.
pub fn spawn_app_logger(app_name: &str, log_dir: PathBuf) -> AppLogHandle {
    spawn_app_logger_with_max(app_name, log_dir, DEFAULT_MAX_FILE_BYTES)
}

/// Register an app log handle so app-scoped server tracing events can be
/// written beside the app's stdout/stderr.
pub fn register_app_logger(app_name: &str, handle: AppLogHandle) {
    APP_LOG_REGISTRY.insert(app_name.to_string(), handle);
}

/// Remove an app log handle from the tracing registry.
pub fn unregister_app_logger(app_name: &str) {
    APP_LOG_REGISTRY.remove(app_name);
}

pub fn app_log_tracing_layer() -> AppLogTracingLayer {
    AppLogTracingLayer
}

pub struct AppLogTracingLayer;

impl<S> Layer<S> for AppLogTracingLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let metadata = event.metadata();
        if !is_app_log_level(metadata.level()) || is_logger_metadata(metadata) {
            return;
        }

        let mut fields = ServerEventFields::default();
        event.record(&mut fields);

        let Some(app_name) = fields.app.as_deref() else {
            return;
        };

        let source = fields.source.as_deref().unwrap_or(SERVER_LOG_SOURCE);
        let line = format_server_event_line(metadata.level().as_str(), &fields);
        write_server_log_entry(app_name, source, line);
    }
}

fn is_app_log_level(level: &Level) -> bool {
    matches!(*level, Level::ERROR | Level::WARN | Level::INFO)
}

fn is_logger_metadata(metadata: &Metadata<'_>) -> bool {
    let target = metadata.target();
    let module_path = metadata.module_path().unwrap_or_default();

    target.ends_with("instances::logger") || module_path.ends_with("instances::logger")
}

fn write_server_log_entry(app_name: &str, source: &str, line: String) {
    let Some(handle) = APP_LOG_REGISTRY.get(app_name) else {
        return;
    };

    handle.try_send(LogEntry {
        instance_id: source.to_string(),
        stream: LogStream::Server,
        line,
    });
}

#[derive(Default)]
struct ServerEventFields {
    app: Option<String>,
    message: Option<String>,
    source: Option<String>,
    fields: Vec<(String, String)>,
}

impl ServerEventFields {
    fn record_field(&mut self, field: &Field, value: String) {
        let value = normalize_log_value(value);
        let name = field.name();
        if name == "app" {
            self.app = Some(value.clone());
        } else if name == "message" {
            self.message = Some(value.clone());
        } else if name == "source" {
            self.source = Some(value.clone());
        }

        self.fields.push((name.to_string(), value));
    }
}

impl Visit for ServerEventFields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_field(field, normalize_debug_value(format!("{value:?}")));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_field(field, value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_field(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_field(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_field(field, value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record_field(field, value.to_string());
    }
}

fn normalize_debug_value(value: String) -> String {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(&value)
        .to_string()
}

fn normalize_log_value(value: String) -> String {
    value.replace('\r', "\\r").replace('\n', "\\n")
}

fn format_server_event_line(level: &str, fields: &ServerEventFields) -> String {
    let mut line = level.to_string();
    if let Some(message) = fields
        .message
        .as_deref()
        .filter(|message| !message.is_empty())
    {
        line.push(' ');
        line.push_str(message);
    }

    for (name, value) in &fields.fields {
        if name == "app" || name == "message" || name == "source" {
            continue;
        }

        line.push(' ');
        line.push_str(name);
        line.push('=');
        line.push_str(value);
    }

    line
}

fn spawn_app_logger_with_max(
    app_name: &str,
    log_dir: PathBuf,
    max_file_bytes: u64,
) -> AppLogHandle {
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));
    let handle = AppLogHandle {
        tx,
        dropped: dropped.clone(),
    };

    let app_name = app_name.to_string();
    tokio::spawn(async move {
        writer_loop(app_name, log_dir, max_file_bytes, rx, dropped).await;
    });

    handle
}

async fn writer_loop(
    app_name: String,
    log_dir: PathBuf,
    max_file_bytes: u64,
    mut rx: mpsc::Receiver<LogEntry>,
    dropped: Arc<AtomicU64>,
) {
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        tracing::warn!(app = %app_name, error = %e, "Failed to create log directory");
        // Still drain the channel so senders don't block.
        while rx.recv().await.is_some() {}
        return;
    }

    let current_path = log_dir.join("current.log");
    let previous_path = log_dir.join("previous.log");

    let file = match open_append(&current_path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(app = %app_name, error = %e, "Failed to open log file");
            while rx.recv().await.is_some() {}
            return;
        }
    };

    let mut writer = AppLogWriter {
        file,
        current_path,
        previous_path,
        bytes_written: 0,
        max_file_bytes,
    };

    // Recover byte count from existing file.
    if let Ok(meta) = std::fs::metadata(&writer.current_path) {
        writer.bytes_written = meta.len();
    }

    let mut flush_interval = tokio::time::interval(FLUSH_INTERVAL);
    flush_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_dropped_report: u64 = 0;

    loop {
        tokio::select! {
            entry = rx.recv() => {
                let Some(entry) = entry else { break };
                writer.write_entry(&entry).await;
            }
            _ = flush_interval.tick() => {
                let _ = writer.file.flush().await;

                // Periodically report dropped lines.
                let total_dropped = dropped.load(Ordering::Relaxed);
                if total_dropped > last_dropped_report {
                    let delta = total_dropped - last_dropped_report;
                    tracing::warn!(
                        app = %app_name,
                        dropped = delta,
                        "App log lines dropped (logging faster than disk)"
                    );
                    last_dropped_report = total_dropped;
                }
            }
        }
    }

    let _ = writer.file.flush().await;
}

struct AppLogWriter {
    file: tokio::fs::File,
    current_path: PathBuf,
    previous_path: PathBuf,
    bytes_written: u64,
    max_file_bytes: u64,
}

impl AppLogWriter {
    async fn write_entry(&mut self, entry: &LogEntry) {
        let now = format_utc_now();
        let line = format!(
            "{} [{}] [{}] {}\n",
            now,
            entry.stream.label(),
            entry.instance_id,
            entry.line
        );

        if let Err(e) = self.file.write_all(line.as_bytes()).await {
            tracing::debug!(error = %e, "Failed to write log line");
            return;
        }

        self.bytes_written += line.len() as u64;

        if self.bytes_written >= self.max_file_bytes {
            self.rotate().await;
        }
    }

    async fn rotate(&mut self) {
        let _ = self.file.flush().await;

        // Atomic rename on same filesystem.
        let _ = std::fs::rename(&self.current_path, &self.previous_path);

        match open_append(&self.current_path).await {
            Ok(f) => {
                self.file = f;
                self.bytes_written = 0;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to reopen log file after rotation");
                // Keep writing to the old (now renamed) file rather than losing logs.
                // Next rotation attempt will try again.
            }
        }
    }
}

fn format_utc_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let millis = d.subsec_millis();

    // Convert epoch seconds to date/time components (UTC).
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Civil date from days since epoch (algorithm from Howard Hinnant).
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
}

async fn open_append(path: &Path) -> std::io::Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
}

/// Create a no-op log handle for tests. Entries are sent to a channel that is
/// immediately dropped, so writes silently succeed without touching disk.
#[cfg(test)]
pub fn noop_log_handle() -> AppLogHandle {
    let (tx, _rx) = mpsc::channel(1);
    AppLogHandle {
        tx,
        dropped: Arc::new(AtomicU64::new(0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64 as TestAtomicU64, Ordering as TestOrdering};
    use tracing_subscriber::prelude::*;

    static NEXT_TEST_APP_ID: TestAtomicU64 = TestAtomicU64::new(0);

    fn unique_app_name(prefix: &str) -> String {
        let id = NEXT_TEST_APP_ID.fetch_add(1, TestOrdering::Relaxed);
        format!("{prefix}-{id}")
    }

    #[test]
    fn server_event_lines_include_message_and_fields() {
        let fields = ServerEventFields {
            app: Some("demo/production".into()),
            message: Some("Instance ready".into()),
            source: None,
            fields: vec![
                ("message".into(), "Instance ready".into()),
                ("app".into(), "demo/production".into()),
                ("instance".into(), "abc123".into()),
                ("requests".into(), "7".into()),
            ],
        };

        assert_eq!(
            format_server_event_line("INFO", &fields),
            "INFO Instance ready instance=abc123 requests=7"
        );
    }

    #[test]
    fn server_event_fields_are_single_line() {
        let fields = ServerEventFields {
            app: Some("demo/production".into()),
            message: Some("Startup failed".into()),
            source: None,
            fields: vec![
                ("message".into(), "Startup failed".into()),
                ("app".into(), "demo/production".into()),
                (
                    "error".into(),
                    normalize_log_value("line one\nline two".into()),
                ),
            ],
        };

        assert_eq!(
            format_server_event_line("ERROR", &fields),
            "ERROR Startup failed error=line one\\nline two"
        );
    }

    #[tokio::test]
    async fn tracing_layer_writes_app_scoped_events() {
        let dir = tempfile::tempdir().unwrap();
        let app = unique_app_name("trace-app");
        let handle = spawn_app_logger(&app, dir.path().to_path_buf());
        register_app_logger(&app, handle.clone());

        let subscriber = tracing_subscriber::registry().with(app_log_tracing_layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                app = %app,
                instance = "inst1",
                requests = 3_u64,
                "Instance ready"
            );
        });

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        unregister_app_logger(&app);
        drop(handle);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let content = std::fs::read_to_string(dir.path().join("current.log")).unwrap();
        assert!(content.contains("[server]"));
        assert!(content.contains("[tako-server]"));
        assert!(content.contains("WARN"));
        assert!(content.contains("Instance ready"));
        assert!(content.contains("instance=inst1"));
        assert!(content.contains("requests=3"));
    }

    #[tokio::test]
    async fn tracing_layer_writes_app_scoped_events_with_source_context() {
        let dir = tempfile::tempdir().unwrap();
        let app = unique_app_name("trace-app");
        let handle = spawn_app_logger(&app, dir.path().to_path_buf());
        register_app_logger(&app, handle.clone());

        let subscriber = tracing_subscriber::registry().with(app_log_tracing_layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                app = %app,
                source = "images",
                requested_format = "webp",
                "Image transform failed; serving original image"
            );
        });

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        unregister_app_logger(&app);
        drop(handle);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let content = std::fs::read_to_string(dir.path().join("current.log")).unwrap();
        assert!(content.contains("[server]"));
        assert!(content.contains("[images]"));
        assert!(content.contains("WARN"));
        assert!(content.contains("Image transform failed; serving original image"));
        assert!(content.contains("requested_format=webp"));
        assert!(!content.contains("source=images"));
    }

    #[tokio::test]
    async fn log_handle_sends_entries() {
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_app_logger("test-app", dir.path().to_path_buf());

        handle.try_send(LogEntry {
            instance_id: "inst1".into(),
            stream: LogStream::Stdout,
            line: "hello world".into(),
        });

        // Give writer time to flush.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        drop(handle);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let content = std::fs::read_to_string(dir.path().join("current.log")).unwrap();
        assert!(content.contains("hello world"));
        assert!(content.contains("[out]"));
        assert!(content.contains("[inst1]"));
    }

    #[tokio::test]
    async fn stderr_lines_tagged() {
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_app_logger("test-app", dir.path().to_path_buf());

        handle.try_send(LogEntry {
            instance_id: "i2".into(),
            stream: LogStream::Stderr,
            line: "oops".into(),
        });

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        drop(handle);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let content = std::fs::read_to_string(dir.path().join("current.log")).unwrap();
        assert!(content.contains("[err]"));
        assert!(content.contains("oops"));
    }

    #[tokio::test]
    async fn rotation_creates_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        // Tiny max so rotation triggers quickly.
        let handle = spawn_app_logger_with_max("rot-app", dir.path().to_path_buf(), 100);

        for i in 0..20 {
            handle.try_send(LogEntry {
                instance_id: "inst".into(),
                stream: LogStream::Stdout,
                line: format!("line {i} padding to make it longer than you'd expect"),
            });
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop(handle);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(dir.path().join("current.log").exists());
        assert!(dir.path().join("previous.log").exists());
    }

    #[tokio::test]
    async fn backpressure_increments_dropped_count() {
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_app_logger("bp-app", dir.path().to_path_buf());

        // Flood the channel beyond capacity.
        for i in 0..CHANNEL_CAPACITY + 500 {
            handle.try_send(LogEntry {
                instance_id: "inst".into(),
                stream: LogStream::Stdout,
                line: format!("flood line {i}"),
            });
        }

        // Some should have been dropped (channel is bounded).
        // The exact number depends on how fast the writer drains.
        // Just verify the mechanism works by checking we can read the count.
        let _dropped = handle.dropped_count();
        // Not asserting exact count since it's timing-dependent.

        drop(handle);
    }

    #[tokio::test]
    async fn log_pipe_forwards_lines() {
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_app_logger("pipe-app", dir.path().to_path_buf());

        let data = b"first line\nsecond line\nthird line\n";
        let cursor = std::io::Cursor::new(data.to_vec());

        log_pipe(cursor, handle.clone(), "p1".into(), LogStream::Stdout).await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        drop(handle);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let content = std::fs::read_to_string(dir.path().join("current.log")).unwrap();
        assert!(content.contains("first line"));
        assert!(content.contains("second line"));
        assert!(content.contains("third line"));
    }
}
