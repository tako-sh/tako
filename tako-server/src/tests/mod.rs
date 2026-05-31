use super::boot::{
    install_rustls_crypto_provider, read_server_config, should_signal_parent_on_ready,
};
use super::release::{
    should_use_self_signed_route_cert, validate_app_name, validate_deploy_routes,
};
use super::{
    Args, SIGNAL_PARENT_ON_READY_ENV, ServerRuntimeConfig, ServerState, extract_zstd_archive,
    run_extract_archive_mode,
};
use crate::instances::AppConfig;
use crate::runtime_events::{handle_idle_event, handle_instance_event};
use crate::socket::{AppState, Command, InstanceState, Response};
use crate::tls::{CertManager, CertManagerConfig, ChallengeTokens};
use clap::Parser;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;
use std::process::{Command as StdCommand, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tako_core::UpgradeMode;
use tempfile::TempDir;
use tracing_subscriber::layer::SubscriberExt as _;

fn empty_challenge_tokens() -> ChallengeTokens {
    Arc::new(parking_lot::RwLock::new(HashMap::new()))
}

fn write_release_manifest(
    release_dir: &Path,
    runtime: &str,
    main: &str,
    start: &[&str],
    install: Option<&str>,
    idle_timeout: u32,
) {
    write_release_manifest_with_app_dir(
        release_dir,
        runtime,
        main,
        start,
        install,
        idle_timeout,
        "",
    );
}

fn write_release_manifest_with_app_dir(
    release_dir: &Path,
    runtime: &str,
    main: &str,
    start: &[&str],
    install: Option<&str>,
    idle_timeout: u32,
    app_dir: &str,
) {
    let mut manifest = serde_json::json!({
        "runtime": runtime,
        "main": main,
        "idle_timeout": idle_timeout,
        "app_dir": app_dir,
    });
    if !start.is_empty() {
        manifest["start"] =
            serde_json::Value::Array(start.iter().map(|value| (*value).into()).collect());
    }
    if let Some(install) = install {
        manifest["install"] = install.into();
    }
    std::fs::write(
        release_dir.join("app.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn write_js_workflow_scaffold(release_dir: &Path) {
    write_js_workflow_scaffold_at(release_dir, "");
}

fn write_js_workflow_scaffold_at(release_dir: &Path, app_dir: &str) {
    let app_path = if app_dir.is_empty() {
        release_dir.to_path_buf()
    } else {
        release_dir.join(app_dir)
    };
    std::fs::create_dir_all(app_path.join("src").join("workflows")).unwrap();
    std::fs::create_dir_all(app_path.join("node_modules/tako.sh/dist/entrypoints")).unwrap();
    std::fs::write(
        app_path.join("node_modules/tako.sh/dist/entrypoints/bun-worker.mjs"),
        "export default {};",
    )
    .unwrap();
}

fn socket_ready(path: &Path) -> bool {
    (0..50).any(|_| {
        let exists = path.exists();
        let is_symlink = std::fs::symlink_metadata(path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        if exists || is_symlink {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
        false
    })
}

#[test]
fn default_server_log_filter_is_warn() {
    assert_eq!(super::DEFAULT_SERVER_LOG_FILTER, "warn");
}

#[derive(Clone)]
struct SharedLogWriter(Arc<Mutex<Vec<u8>>>);

struct SharedLogBuffer(Arc<Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLogWriter {
    type Writer = SharedLogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        SharedLogBuffer(self.0.clone())
    }
}

impl std::io::Write for SharedLogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn server_json_log_format_includes_release_identity_fields() {
    let output = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .event_format(super::ServerJsonLogFormat::new("0.0.0-test123", 4242))
            .with_writer(SharedLogWriter(output.clone())),
    );

    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!(answer = 42, ready = true, "hello");
    });

    let bytes = output.lock().unwrap().clone();
    let line = String::from_utf8(bytes).unwrap();
    let value: Value = serde_json::from_str(line.trim()).unwrap();

    assert_eq!(value["level"], "WARN");
    assert_eq!(value["server_version"], "0.0.0-test123");
    assert_eq!(value["pid"], 4242);
    assert_eq!(value["fields"]["message"], "hello");
    assert_eq!(value["fields"]["answer"], 42);
    assert_eq!(value["fields"]["ready"], true);
    assert!(value.get("server_name").is_none());
}

fn write_test_release_archive(archive_path: &Path) {
    let file = std::fs::File::create(archive_path).unwrap();
    let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let mut archive = tar::Builder::new(encoder);
    let manifest = br#"{"runtime":"bun","main":"src/index.ts","idle_timeout":300,"app_dir":""}"#;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "app.json", &mut Cursor::new(manifest))
        .unwrap();
    let encoder = archive.into_inner().unwrap();
    encoder.finish().unwrap();
}

fn signal_parent_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// Install flow tests are covered by e2e tests (e2e/fixtures/javascript/*).

fn python3_ok() -> bool {
    StdCommand::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn python3_can_bind_loopback_tcp() -> bool {
    let Some(port) = pick_free_port() else {
        return false;
    };
    StdCommand::new("python3")
        .args([
            "-c",
            "import socket, sys; s = socket.socket(); s.bind(('127.0.0.1', int(sys.argv[1]))); s.close()",
        ])
        .arg(port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn pick_free_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok().map(|a| a.port()))
}

mod archive_boot;
mod delete_upgrade;
mod deploy;
mod lifecycle;
mod release_command;
mod scale_limits;
mod secrets_restore;
mod workflows;
