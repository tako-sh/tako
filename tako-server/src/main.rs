#[cfg(not(unix))]
compile_error!("tako-server requires Unix (management commands use Unix sockets).");

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod app_command;
mod archive;
mod boot;
mod channels;
mod channels_ws;
mod defaults;
mod identity;
mod image_worker;
mod instances;
mod lb;
mod management_auth;
mod management_http;
mod metrics;
mod object_storage;
mod operations;
mod paths;
mod proxy;
mod release;
mod release_command;
mod routing;
mod runtime_events;
mod scaling;
mod server_state;
mod socket;
mod startup;
mod state_store;
mod tls;
mod unix;
mod version_manager;

use tako_workflows as workflows;

use crate::boot::install_rustls_crypto_provider;
use clap::Parser;
#[cfg(any(not(debug_assertions), test))]
use serde_json::{Map, Number, Value};
#[cfg(any(not(debug_assertions), test))]
use std::fmt;
use std::path::Path;
#[cfg(any(not(debug_assertions), test))]
use tracing::field::{Field, Visit};
#[cfg(any(not(debug_assertions), test))]
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::LevelFilter;
#[cfg(any(not(debug_assertions), test))]
use tracing_subscriber::fmt::FmtContext;
#[cfg(any(not(debug_assertions), test))]
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
#[cfg(any(not(debug_assertions), test))]
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::layer::SubscriberExt;
#[cfg(any(not(debug_assertions), test))]
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

pub(crate) use crate::archive::extract_zstd_archive;
pub(crate) use crate::release::is_private_local_hostname;
pub use server_state::{ServerRuntimeConfig, ServerState};

const DEFAULT_SERVER_LOG_FILTER: &str = "warn";
const SIGNAL_PARENT_ON_READY_ENV: &str = "TAKO_SIGNAL_PARENT_ON_READY";

fn server_version() -> &'static str {
    static VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let base = env!("CARGO_PKG_VERSION");
        match option_env!("TAKO_BUILD_SHA") {
            Some(sha) if !sha.trim().is_empty() => {
                let short = &sha.trim()[..sha.trim().len().min(7)];
                format!("{base}-{short}")
            }
            _ => base.to_string(),
        }
    });
    &VERSION
}

#[cfg(any(not(debug_assertions), test))]
#[derive(Clone)]
struct ServerJsonLogFormat {
    server_version: &'static str,
    pid: u32,
}

#[cfg(any(not(debug_assertions), test))]
impl ServerJsonLogFormat {
    fn new(server_version: &'static str, pid: u32) -> Self {
        Self {
            server_version,
            pid,
        }
    }
}

#[cfg(any(not(debug_assertions), test))]
impl<S, N> FormatEvent<S, N> for ServerJsonLogFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut timestamp = String::new();
        SystemTime.format_time(&mut Writer::new(&mut timestamp))?;

        let mut fields = Map::new();
        event.record(&mut JsonFieldVisitor {
            fields: &mut fields,
        });

        let mut entry = Map::new();
        entry.insert("timestamp".to_string(), Value::String(timestamp));
        entry.insert(
            "level".to_string(),
            Value::String(event.metadata().level().to_string()),
        );
        entry.insert(
            "server_version".to_string(),
            Value::String(self.server_version.to_string()),
        );
        entry.insert("pid".to_string(), Value::Number(Number::from(self.pid)));
        entry.insert("fields".to_string(), Value::Object(fields));

        let line = serde_json::to_string(&Value::Object(entry)).map_err(|_| fmt::Error)?;
        writeln!(writer, "{line}")
    }
}

#[cfg(any(not(debug_assertions), test))]
struct JsonFieldVisitor<'a> {
    fields: &'a mut Map<String, Value>,
}

#[cfg(any(not(debug_assertions), test))]
impl JsonFieldVisitor<'_> {
    fn insert(&mut self, field: &Field, value: Value) {
        self.fields.insert(field.name().to_string(), value);
    }
}

#[cfg(any(not(debug_assertions), test))]
impl Visit for JsonFieldVisitor<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        if let Some(number) = Number::from_f64(value) {
            self.insert(field, Value::Number(number));
        } else {
            self.insert(field, Value::String(value.to_string()));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, Value::Number(Number::from(value)));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, Value::Number(Number::from(value)));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        match i64::try_from(value) {
            Ok(value) => self.record_i64(field, value),
            Err(_) => self.insert(field, Value::String(value.to_string())),
        }
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        match u64::try_from(value) {
            Ok(value) => self.record_u64(field, value),
            Err(_) => self.insert(field, Value::String(value.to_string())),
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, Value::Bool(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, Value::String(value.to_string()));
    }

    fn record_bytes(&mut self, field: &Field, value: &[u8]) {
        self.insert(field, Value::String(format!("{value:02x?}")));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.insert(field, Value::String(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.insert(field, Value::String(format!("{value:?}")));
    }
}

/// Tako Server - Application runtime and proxy
#[derive(Parser)]
#[command(name = "tako-server")]
#[command(version = server_version())]
#[command(about = "Tako Server - Application runtime and proxy")]
pub struct Args {
    /// Unix socket path for management commands
    #[arg(long)]
    pub socket: Option<String>,

    /// HTTP port
    #[arg(long = "http-port", default_value_t = 80)]
    pub http_port: u16,

    /// HTTPS port
    #[arg(long = "https-port", default_value_t = 443)]
    pub https_port: u16,

    /// Use Let's Encrypt staging environment
    #[arg(long)]
    pub acme_staging: bool,

    /// Data directory for apps and certificates
    #[arg(long)]
    pub data_dir: Option<String>,

    /// Disable ACME (use self-signed or manual certificates only)
    #[arg(long)]
    pub no_acme: bool,

    /// Certificate renewal check interval in hours (default: 12)
    #[arg(long, default_value_t = 12)]
    pub renewal_interval_hours: u64,

    /// Run as a hot standby: serve traffic with minimal scaling (max 1 instance
    /// per app), skip management socket and ACME. Monitors the primary
    /// server's socket — promotes to full mode if primary is unavailable,
    /// shuts down gracefully when primary comes back.
    #[arg(long)]
    pub standby: bool,

    /// Prometheus metrics port (default: 9898, set to 0 to disable)
    #[arg(long, default_value_t = 9898)]
    pub metrics_port: u16,

    /// Private host/IP to bind remote management HTTP on (port 9844).
    #[arg(long)]
    pub management_host: Option<String>,

    /// Extract a `.tar.zst` archive into a destination directory and exit.
    #[arg(long, hide = true)]
    pub extract_zstd_archive: Option<String>,

    /// Destination directory used with `--extract-zstd-archive`.
    #[arg(long, hide = true)]
    pub extract_dest: Option<String>,

    /// Run the isolated image transform worker protocol on stdin/stdout.
    #[arg(long, hide = true)]
    pub image_worker: bool,
}

fn run_extract_archive_mode(args: &Args) -> Result<(), String> {
    let archive = args
        .extract_zstd_archive
        .as_deref()
        .ok_or_else(|| "Extraction mode requires --extract-zstd-archive <path>".to_string())?;
    let dest = args
        .extract_dest
        .as_deref()
        .ok_or_else(|| "Extraction mode requires --extract-dest <dir>".to_string())?;
    extract_zstd_archive(Path::new(archive), Path::new(dest))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    install_rustls_crypto_provider();

    let args = Args::parse();
    if args.image_worker {
        image_worker::run_stdio().map_err(std::io::Error::other)?;
        return Ok(());
    }

    // Initialize tracing with a non-blocking writer so log I/O never stalls
    // Tokio worker threads (critical under high request volume / DDoS).
    let (non_blocking, _guard) = tracing_appender::non_blocking(std::io::stdout());
    let stdout_filter = || {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(DEFAULT_SERVER_LOG_FILTER))
    };

    #[cfg(debug_assertions)]
    tracing_subscriber::registry()
        .with(instances::app_log_tracing_layer().with_filter(LevelFilter::INFO))
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_target(false)
                .with_writer(non_blocking)
                .with_filter(stdout_filter()),
        )
        .init();

    #[cfg(not(debug_assertions))]
    tracing_subscriber::registry()
        .with(instances::app_log_tracing_layer().with_filter(LevelFilter::INFO))
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(ServerJsonLogFormat::new(
                    server_version(),
                    std::process::id(),
                ))
                .with_writer(non_blocking)
                .with_filter(stdout_filter()),
        )
        .init();

    if args.extract_zstd_archive.is_some() || args.extract_dest.is_some() {
        run_extract_archive_mode(&args)?;
        return Ok(());
    }
    startup::run(args)
}

#[cfg(test)]
mod tests;
