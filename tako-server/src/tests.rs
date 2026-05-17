use super::boot::{
    install_rustls_crypto_provider, read_server_config, should_signal_parent_on_ready,
};
use super::release::{
    should_use_self_signed_route_cert, validate_app_name, validate_deploy_routes,
};
use super::{
    SIGNAL_PARENT_ON_READY_ENV, ServerRuntimeConfig, ServerState, extract_zstd_archive,
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

#[test]
fn extract_zstd_archive_unpacks_files() {
    let temp = TempDir::new().unwrap();
    let archive_path = temp.path().join("payload.tar.zst");
    let dest = temp.path().join("dest");

    let file = std::fs::File::create(&archive_path).unwrap();
    let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let mut archive = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    let payload = b"hello";
    header.set_size(payload.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "app/index.txt", &mut Cursor::new(payload))
        .unwrap();
    let encoder = archive.into_inner().unwrap();
    encoder.finish().unwrap();

    extract_zstd_archive(&archive_path, &dest).unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.join("app/index.txt")).unwrap(),
        "hello"
    );
}

#[test]
fn extract_zstd_archive_rejects_path_traversal() {
    let temp = TempDir::new().unwrap();
    let archive_path = temp.path().join("malicious.tar.zst");
    let dest = temp.path().join("dest");

    // Build a tar with a `../escape.txt` entry by writing raw header bytes,
    // bypassing the builder's own path validation.
    let file = std::fs::File::create(&archive_path).unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    {
        use std::io::Write;
        let mut header = tar::Header::new_gnu();
        let payload = b"pwned";
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        // Write path directly into the header name field
        let path = b"../escape.txt";
        let bytes = header.as_mut_bytes();
        bytes[..path.len()].copy_from_slice(path);
        header.set_cksum();

        encoder.write_all(header.as_bytes()).unwrap();
        encoder.write_all(payload).unwrap();
        // Pad to 512-byte boundary
        let padding = 512 - (payload.len() % 512);
        if padding < 512 {
            encoder.write_all(&vec![0u8; padding]).unwrap();
        }
        // Two zero blocks to end archive
        encoder.write_all(&[0u8; 1024]).unwrap();
    }
    encoder.finish().unwrap();

    // tar crate silently skips entries with `..` (returns Ok)
    extract_zstd_archive(&archive_path, &dest).unwrap();
    assert!(
        !temp.path().join("escape.txt").exists(),
        "path traversal: file escaped dest"
    );
    assert!(
        !dest.join("escape.txt").exists(),
        "path traversal: file should be skipped entirely"
    );
}

#[test]
fn run_extract_archive_mode_requires_destination_flag() {
    let args = super::Args::try_parse_from([
        "tako-server",
        "--extract-zstd-archive",
        "/tmp/payload.tar.zst",
    ])
    .unwrap();
    let err = run_extract_archive_mode(&args).unwrap_err();
    assert!(err.contains("--extract-dest"));
}

#[test]
fn install_rustls_crypto_provider_is_idempotent() {
    install_rustls_crypto_provider();
    assert!(rustls::crypto::CryptoProvider::get_default().is_some());

    install_rustls_crypto_provider();
    assert!(rustls::crypto::CryptoProvider::get_default().is_some());
}

fn signal_parent_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn should_signal_parent_on_ready_defaults_to_false() {
    let _guard = signal_parent_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    unsafe {
        std::env::remove_var(SIGNAL_PARENT_ON_READY_ENV);
    }
    assert!(!should_signal_parent_on_ready());
}

#[test]
fn should_signal_parent_on_ready_reads_env_toggle() {
    let _guard = signal_parent_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    unsafe {
        std::env::set_var(SIGNAL_PARENT_ON_READY_ENV, "1");
    }
    assert!(should_signal_parent_on_ready());

    unsafe {
        std::env::set_var(SIGNAL_PARENT_ON_READY_ENV, "0");
    }
    assert!(!should_signal_parent_on_ready());

    unsafe {
        std::env::remove_var(SIGNAL_PARENT_ON_READY_ENV);
    }
}

#[test]
fn validate_deploy_routes_rejects_empty_routes() {
    let err = validate_deploy_routes(&[]).unwrap_err();
    assert!(err.contains("at least one route"));
}

#[test]
fn validate_deploy_routes_rejects_empty_route_entry() {
    let err = validate_deploy_routes(&["".to_string()]).unwrap_err();
    assert!(err.contains("non-empty"));
}

#[test]
fn validate_app_name_accepts_app_env_identifier() {
    assert!(validate_app_name("my-app/staging").is_ok());
}

#[tokio::test]
async fn deploy_rejects_invalid_app_name() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let response = state
        .handle_command(Command::Deploy {
            app: "../escape".to_string(),
            version: "v1".to_string(),
            path: temp.path().to_string_lossy().to_string(),
            routes: vec!["api.example.com".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            storages: Some(HashMap::new()),
            dns: None,
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected invalid app name to be rejected");
    };
    assert!(message.contains("Invalid app name"), "got: {message}");
}

#[tokio::test]
async fn state_store_persists_dns_credentials_per_app() {
    let temp_dir = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp_dir.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp_dir.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let dns = tako_core::DnsBinding {
        provider: tako_core::DnsProvider::Cloudflare,
        cloudflare_api_token: Some("token-a".to_string()),
    };

    state
        .state_store
        .set_dns("my-app/production", &dns)
        .unwrap();

    assert_eq!(
        state.state_store.get_dns("my-app/production").unwrap(),
        Some(dns)
    );
    assert_eq!(
        state.state_store.get_dns("other-app/production").unwrap(),
        None
    );
}

#[tokio::test]
async fn failed_deploy_does_not_persist_dns_credentials() {
    let temp_dir = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp_dir.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp_dir.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let app_id = "my-app/production";
    let release_dir = temp_dir
        .path()
        .join("apps")
        .join("my-app")
        .join("production")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    write_release_manifest(
        &release_dir,
        "node",
        "",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    let response = state
        .handle_command(Command::Deploy {
            app: app_id.to_string(),
            version: "v1".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            routes: vec!["*.example.com".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            storages: Some(HashMap::new()),
            dns: Some(tako_core::DnsBinding {
                provider: tako_core::DnsProvider::Cloudflare,
                cloudflare_api_token: Some("token".to_string()),
            }),
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected invalid release to be rejected");
    };
    assert!(message.contains("empty main field"), "got: {message}");
    assert_eq!(state.state_store.get_dns(app_id).unwrap(), None);
}

#[tokio::test]
async fn deploy_rejects_release_path_outside_managed_root() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let outside_release = temp.path().join("outside-release");
    std::fs::create_dir_all(&outside_release).unwrap();

    let response = state
        .handle_command(Command::Deploy {
            app: "demo-app".to_string(),
            version: "v1".to_string(),
            path: outside_release.to_string_lossy().to_string(),
            routes: vec!["api.example.com".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            storages: Some(HashMap::new()),
            dns: None,
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected out-of-root deploy path to be rejected");
    };
    assert!(
        message.contains("Invalid release path"),
        "expected path validation error, got: {message}"
    );
}

#[tokio::test]
async fn deploy_rejects_invalid_release_version() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release_dir = temp
        .path()
        .join("apps")
        .join("demo-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();

    let response = state
        .handle_command(Command::Deploy {
            app: "demo-app".to_string(),
            version: "../v1".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            routes: vec!["api.example.com".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            storages: Some(HashMap::new()),
            dns: None,
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected invalid release version to be rejected");
    };
    assert!(
        message.contains("Invalid release version"),
        "got: {message}"
    );
}

#[test]
fn private_route_domains_prefer_self_signed_certs() {
    assert!(should_use_self_signed_route_cert(
        "tako-bun-server.orb.local"
    ));
    assert!(should_use_self_signed_route_cert("localhost"));
    assert!(should_use_self_signed_route_cert("api.localhost"));
    assert!(should_use_self_signed_route_cert("my-service"));
}

#[test]
fn public_route_domains_do_not_prefer_self_signed_certs() {
    assert!(!should_use_self_signed_route_cert("api.example.com"));
    assert!(!should_use_self_signed_route_cert("example.com"));
}

#[test]
fn bun_runtime_has_install_script() {
    let runtime = tako_runtime::runtime_def_for("bun", None).unwrap();
    let install = runtime.package_manager.install.as_deref().unwrap();
    assert!(install.contains("bun install --production"));
}

#[test]
fn node_runtime_uses_npm_install_script() {
    let runtime = tako_runtime::runtime_def_for("node", None).unwrap();
    let install = runtime.package_manager.install.as_deref().unwrap();
    assert!(install.contains("npm"));
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

#[tokio::test]
async fn ensure_route_certificate_generates_self_signed_for_private_domain() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    cert_manager.init().unwrap();
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager.clone(),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let cert = state
        .ensure_route_certificate("my-app", "tako-bun-server.orb.local")
        .await
        .expect("private domain should get a generated cert");
    assert!(cert.is_self_signed);
    assert_eq!(cert.domain, "tako-bun-server.orb.local");

    let cached = cert_manager
        .get_cert_for_host("tako-bun-server.orb.local")
        .expect("generated cert should be cached");
    assert!(cached.is_self_signed);
}

#[tokio::test]
async fn delete_command_removes_runtime_registration_and_routes() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let app_root = temp.path().join("apps").join("my-app");
    let release_dir = app_root.join("releases").join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    std::fs::create_dir_all(app_root.join("data/app")).unwrap();
    std::fs::create_dir_all(app_root.join("data/tako")).unwrap();

    let config = AppConfig {
        name: "my-app".to_string(),
        version: "v1".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "exit 0".to_string(),
        ],
        min_instances: 0,
        ..Default::default()
    };

    let app = state.app_manager.register_app(config);
    state.load_balancer.register_app(app);
    {
        let mut route_table = state.routes.write().await;
        route_table.set_app_routes("my-app".to_string(), vec!["api.example.com".to_string()]);
    }

    let response = state
        .handle_command(Command::Delete {
            app: "my-app".to_string(),
        })
        .await;
    assert!(matches!(response, Response::Ok { .. }));
    assert!(state.app_manager.get_app("my-app").is_none());
    assert!(!app_root.exists());

    let route_table = state.routes.read().await;
    assert!(route_table.routes_for_app("my-app").is_empty());
    assert_eq!(route_table.select("api.example.com", "/"), None);
}

#[tokio::test]
async fn delete_command_is_idempotent_for_missing_app() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let response = state
        .handle_command(Command::Delete {
            app: "missing-app".to_string(),
        })
        .await;
    assert!(matches!(response, Response::Ok { .. }));
    assert!(state.app_manager.get_app("missing-app").is_none());
}

#[tokio::test]
async fn delete_command_rejects_invalid_app_name() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let response = state
        .handle_command(Command::Delete {
            app: "../bad".to_string(),
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected invalid app name to be rejected");
    };
    assert!(message.contains("Invalid app name"), "got: {message}");
}

#[tokio::test]
async fn upgrading_mode_blocks_mutating_commands() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    state.set_server_mode(UpgradeMode::Upgrading).await.unwrap();

    let response = state
        .handle_command(Command::Delete {
            app: "my-app".to_string(),
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected blocked mutating command while upgrading");
    };
    assert!(message.contains("Server is upgrading"));
    assert!(message.contains("delete"));
}

#[tokio::test]
async fn server_mode_resets_upgrading_on_boot() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));

    let state_a = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager.clone(),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    state_a
        .set_server_mode(UpgradeMode::Upgrading)
        .await
        .unwrap();
    // Simulate an upgrade lock left behind by a crashed CLI.
    assert!(state_a.try_enter_upgrading("crashed-cli").await.unwrap());
    drop(state_a);

    // On restart, stale Upgrading mode AND orphaned lock should be cleared.
    let state_b = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager.clone(),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    assert_eq!(*state_b.server_mode.read().await, UpgradeMode::Normal);
    // A new owner should be able to acquire immediately (no 10-min stale wait).
    assert!(state_b.try_enter_upgrading("new-cli").await.unwrap());
}

#[tokio::test]
async fn upgrading_lock_allows_single_owner() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state_a = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager.clone(),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let state_b = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    assert!(state_a.try_enter_upgrading("controller-a").await.unwrap());
    assert!(!state_b.try_enter_upgrading("controller-b").await.unwrap());
    assert!(state_a.exit_upgrading("controller-a").await.unwrap());
    assert!(state_b.try_enter_upgrading("controller-b").await.unwrap());
}

#[tokio::test]
async fn server_info_command_reports_runtime_config() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let runtime = ServerRuntimeConfig {
        pid: std::process::id(),
        process_started_at_unix_secs: Some(1_778_220_000),
        socket: "/var/run/tako/tako-custom.sock".to_string(),
        data_dir: temp.path().to_path_buf(),
        http_port: 8080,
        https_port: 8443,
        no_acme: true,
        acme_staging: false,
        renewal_interval_hours: 24,
        standby: false,
        metrics_port: Some(9898),
        server_name: Some("test-server".to_string()),
        server_identity: Some("SHA256:testidentity".to_string()),
    };
    let state = ServerState::new_with_runtime(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
        runtime,
    )
    .unwrap();
    state
        .set_server_mode(UpgradeMode::Upgrading)
        .await
        .expect("mode set");

    let response = state.handle_command(Command::ServerInfo).await;
    let Response::Ok { data } = response else {
        panic!("expected server info response");
    };
    assert_eq!(
        data.get("pid").and_then(Value::as_u64),
        Some(std::process::id() as u64)
    );
    assert_eq!(data.get("mode").and_then(Value::as_str), Some("upgrading"));
    assert_eq!(
        data.get("socket").and_then(Value::as_str),
        Some("/var/run/tako/tako-custom.sock")
    );
    assert_eq!(data.get("http_port").and_then(Value::as_u64), Some(8080));
    assert_eq!(data.get("https_port").and_then(Value::as_u64), Some(8443));
    assert_eq!(data.get("no_acme").and_then(Value::as_bool), Some(true));
    assert_eq!(
        data.get("server_identity").and_then(Value::as_str),
        Some("SHA256:testidentity")
    );
}

#[tokio::test]
async fn enter_and_exit_upgrading_commands_use_owner_lock() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let enter = state
        .handle_command(Command::EnterUpgrading {
            owner: "controller-a".to_string(),
        })
        .await;
    assert!(matches!(enter, Response::Ok { .. }));

    let reject = state
        .handle_command(Command::EnterUpgrading {
            owner: "controller-b".to_string(),
        })
        .await;
    let Response::Error { message } = reject else {
        panic!("expected lock owner rejection");
    };
    assert!(message.contains("already upgrading"));
    assert!(message.contains("controller-a"));

    let wrong_exit = state
        .handle_command(Command::ExitUpgrading {
            owner: "controller-b".to_string(),
        })
        .await;
    assert!(matches!(wrong_exit, Response::Error { .. }));

    let exit = state
        .handle_command(Command::ExitUpgrading {
            owner: "controller-a".to_string(),
        })
        .await;
    assert!(matches!(exit, Response::Ok { .. }));
}

#[tokio::test]
async fn get_secrets_hash_returns_hash_of_app_secrets() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    // No secrets file → hash of empty map
    let response = state
        .handle_command(Command::GetSecretsHash {
            app: "my-app".to_string(),
        })
        .await;
    let Response::Ok { data } = &response else {
        panic!("expected ok response: {response:?}");
    };
    let empty_hash = data.get("hash").and_then(Value::as_str).unwrap();
    assert_eq!(empty_hash, tako_core::compute_secrets_hash(&HashMap::new()));

    // Store secrets and check hash changes
    let secrets: HashMap<String, String> = [("KEY".to_string(), "val".to_string())]
        .into_iter()
        .collect();
    state.state_store.set_secrets("my-app", &secrets).unwrap();

    let response = state
        .handle_command(Command::GetSecretsHash {
            app: "my-app".to_string(),
        })
        .await;
    let Response::Ok { data } = &response else {
        panic!("expected ok response");
    };
    let with_secrets_hash = data.get("hash").and_then(Value::as_str).unwrap();
    assert_ne!(with_secrets_hash, empty_hash);
    assert_eq!(with_secrets_hash, tako_core::compute_secrets_hash(&secrets));
}

#[tokio::test]
async fn deploy_without_secrets_keeps_existing() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    // Pre-store secrets for the app
    let secrets: HashMap<String, String> = [("API_KEY".to_string(), "original".to_string())]
        .into_iter()
        .collect();
    state.state_store.set_secrets("keep-app", &secrets).unwrap();

    let release_dir = temp
        .path()
        .join("apps")
        .join("keep-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    write_release_manifest(
        &release_dir,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    // Deploy with secrets: None — should keep existing
    let _response = state
        .handle_command(Command::Deploy {
            app: "keep-app".to_string(),
            version: "v1".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            routes: vec!["keep.localhost".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: None,
            storages: None,
            dns: None,
        })
        .await;

    // Verify secrets still have original value
    let loaded = state.state_store.get_secrets("keep-app").unwrap();
    assert_eq!(loaded.get("API_KEY"), Some(&"original".to_string()));
}

#[tokio::test]
async fn failed_deploy_does_not_persist_credentials_for_unregistered_app() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release_dir = temp
        .path()
        .join("apps")
        .join("bad-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    std::fs::write(
        release_dir.join("app.json"),
        r#"{"runtime":"python","main":"server.py","idle_timeout":300}"#,
    )
    .unwrap();

    let secrets: HashMap<String, String> = [("API_KEY".to_string(), "new".to_string())]
        .into_iter()
        .collect();
    let storages: HashMap<String, tako_core::StorageBinding> = [(
        "uploads".to_string(),
        tako_core::StorageBinding {
            provider: tako_core::StorageProvider::Local,
            bucket: None,
            endpoint: None,
            region: None,
            access_key_id: None,
            secret_access_key: None,
            force_path_style: false,
            public_base_url: None,
            path: Some("uploads".to_string()),
            signing_key: None,
        },
    )]
    .into_iter()
    .collect();

    let response = state
        .handle_command(Command::Deploy {
            app: "bad-app".to_string(),
            version: "v1".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            routes: vec!["bad.localhost".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(secrets),
            storages: Some(storages),
            dns: None,
        })
        .await;

    assert!(
        matches!(response, Response::Error { .. }),
        "expected unsupported runtime deploy failure: {response:?}"
    );
    assert!(state.state_store.get_secrets("bad-app").unwrap().is_empty());
    assert!(
        state
            .state_store
            .get_storages("bad-app")
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn restore_from_state_store_rehydrates_apps_routes_and_secrets() {
    let temp = TempDir::new().unwrap();
    let app_id = "my-app/production";
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));

    let state_a = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager.clone(),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let release_dir = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("production")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    write_release_manifest(
        &release_dir,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    let app_secrets: HashMap<String, String> =
        [("DATABASE_URL".to_string(), "postgres://db".to_string())]
            .into_iter()
            .collect();
    state_a
        .state_store
        .set_secrets(app_id, &app_secrets)
        .unwrap();

    let app = state_a.app_manager.register_app(AppConfig {
        name: "my-app".to_string(),
        environment: "production".to_string(),
        version: "v1".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "sleep 600".to_string(),
        ],
        min_instances: 0,
        max_instances: 4,
        source_ip: tako_core::SourceIpMode::CloudflareProxy,
        idle_timeout: Duration::from_secs(300),
        ..Default::default()
    });
    state_a.load_balancer.register_app(app);
    {
        let mut route_table = state_a.routes.write().await;
        route_table.set_app_routes_with_source_ip(
            app_id.to_string(),
            vec![
                "api.example.com".to_string(),
                "example.com/api/*".to_string(),
            ],
            tako_core::SourceIpMode::CloudflareProxy,
        );
    }
    state_a.persist_app_state(app_id).await;
    drop(state_a);

    let state_b = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    state_b.restore_from_state_store().await.unwrap();

    let restored = state_b.app_manager.get_app(app_id).expect("app restored");
    assert_eq!(restored.version(), "v1");
    assert_eq!(
        restored.config.read().source_ip,
        tako_core::SourceIpMode::CloudflareProxy
    );
    assert_eq!(restored.state(), crate::socket::AppState::Idle);
    let route_table = state_b.routes.read().await;
    assert_eq!(
        route_table.routes_for_app(app_id),
        vec![
            "api.example.com".to_string(),
            "example.com/api/*".to_string()
        ]
    );
    assert_eq!(
        route_table
            .select_with_route("api.example.com", "/")
            .expect("route restored")
            .source_ip,
        tako_core::SourceIpMode::CloudflareProxy
    );
    let restored_secrets = restored.config.read().secrets.clone();
    assert_eq!(
        restored_secrets.get("DATABASE_URL"),
        Some(&"postgres://db".to_string())
    );
}

#[tokio::test]
async fn restore_from_state_store_restarts_internal_socket_for_apps_with_workflows() {
    let temp = TempDir::new().unwrap();
    let app_id = "workflow-app/production";
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));

    let state_a = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager.clone(),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let release_dir = temp
        .path()
        .join("apps")
        .join("workflow-app")
        .join("production")
        .join("releases")
        .join("v1");
    write_js_workflow_scaffold(&release_dir);
    assert!(release_dir.join("src").join("workflows").is_dir());
    assert!(
        release_dir
            .join("node_modules")
            .join("tako.sh")
            .join("dist")
            .join("entrypoints")
            .join("bun-worker.mjs")
            .is_file()
    );
    write_release_manifest(
        &release_dir,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    let app = state_a.app_manager.register_app(AppConfig {
        name: "workflow-app".to_string(),
        environment: "production".to_string(),
        version: "v1".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "sleep 600".to_string(),
        ],
        min_instances: 0,
        max_instances: 4,
        idle_timeout: Duration::from_secs(300),
        ..Default::default()
    });
    state_a.load_balancer.register_app(app);
    state_a.persist_app_state(app_id).await;
    drop(state_a);

    let state_b = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    state_b.restore_from_state_store().await.unwrap();

    assert!(
        state_b.app_manager.get_app(app_id).is_some(),
        "restored workflow app should be present in the app manager"
    );
    assert!(
        state_b.workflows.has(app_id),
        "restored workflow app should be re-registered with the workflow manager"
    );

    let socket = state_b.workflows.socket_path();
    let socket_ready = socket_ready(&socket);
    assert!(
        socket_ready,
        "restored workflow apps must restart the shared internal socket at {}",
        socket.display()
    );
}

#[tokio::test]
async fn server_state_starts_internal_socket_at_boot() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));

    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let socket = state.workflows.socket_path();
    assert!(
        socket_ready(&socket),
        "server boot must start the shared internal socket at {} so app-side channel .publish() works without workflows/",
        socket.display()
    );
}

#[test]
fn server_state_new_outside_tokio_runtime_does_not_panic() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));

    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .expect("server state should initialize without an entered Tokio runtime");

    assert_eq!(
        state.workflows.socket_path(),
        temp.path().join("internal.sock")
    );
}

#[tokio::test]
async fn sync_app_workflows_restarts_existing_entry_and_stops_removed_workflows() {
    let temp = TempDir::new().unwrap();
    let app_id = "workflow-app/production";
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release_v1 = temp
        .path()
        .join("apps")
        .join("workflow-app")
        .join("production")
        .join("releases")
        .join("v1");
    write_js_workflow_scaffold(&release_v1);
    write_release_manifest(
        &release_v1,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    state.sync_app_workflows(app_id, &release_v1, None).await;
    let first = state
        .workflows
        .supervisor_for(app_id)
        .expect("v1 should register workflows");

    let release_v2 = temp
        .path()
        .join("apps")
        .join("workflow-app")
        .join("production")
        .join("releases")
        .join("v2");
    write_js_workflow_scaffold(&release_v2);
    write_release_manifest(
        &release_v2,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    state.sync_app_workflows(app_id, &release_v2, None).await;
    let second = state
        .workflows
        .supervisor_for(app_id)
        .expect("v2 should replace workflows");
    assert!(
        !Arc::ptr_eq(&first, &second),
        "redeploy should replace the workflow supervisor"
    );

    let release_v3 = temp
        .path()
        .join("apps")
        .join("workflow-app")
        .join("production")
        .join("releases")
        .join("v3");
    std::fs::create_dir_all(&release_v3).unwrap();
    write_release_manifest(
        &release_v3,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    state.sync_app_workflows(app_id, &release_v3, None).await;
    assert!(
        !state.workflows.has(app_id),
        "deploying a release without workflows/ should stop the old workflow runtime"
    );
}

#[tokio::test]
async fn sync_app_workflows_uses_manifest_app_root() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    for (index, (app_root, workflows_dir)) in [(".", "workflows"), ("app", "app/workflows")]
        .into_iter()
        .enumerate()
    {
        let app_id = format!("workflow-app-{index}/production");
        let release = temp
            .path()
            .join("apps")
            .join(format!("workflow-app-{index}"))
            .join("production")
            .join("releases")
            .join("v1");
        std::fs::create_dir_all(release.join(workflows_dir)).unwrap();
        std::fs::create_dir_all(release.join("node_modules/tako.sh/dist/entrypoints")).unwrap();
        std::fs::write(
            release.join("node_modules/tako.sh/dist/entrypoints/bun-worker.mjs"),
            "export default {};",
        )
        .unwrap();
        std::fs::write(
            release.join("app.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "runtime": "bun",
                "main": "index.js",
                "idle_timeout": 300,
                "env_vars": {
                    "TAKO_APP_ROOT": app_root
                }
            }))
            .unwrap(),
        )
        .unwrap();

        state.sync_app_workflows(&app_id, &release, None).await;
        assert!(
            state.workflows.has(&app_id),
            "release with TAKO_APP_ROOT={app_root:?} should register workflows"
        );
    }
}

#[tokio::test]
async fn sync_app_workflows_respects_manifest_app_dir_for_workspace_layouts() {
    let temp = TempDir::new().unwrap();
    let app_id = "demo/production";
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release = temp
        .path()
        .join("apps")
        .join("demo")
        .join("production")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release).unwrap();
    let app_dir = "examples/javascript/demo";
    write_js_workflow_scaffold_at(&release, app_dir);
    write_release_manifest_with_app_dir(
        &release,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
        app_dir,
    );

    state.sync_app_workflows(app_id, &release, None).await;
    assert!(
        state.workflows.has(app_id),
        "workspace-layout deploys should register workflows using manifest.app_dir"
    );
}

#[tokio::test]
async fn sync_app_workflows_injects_release_env_and_app_data_dir_into_worker() {
    let temp = TempDir::new().unwrap();
    let app_id = "workflow-app/production";
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release = temp
        .path()
        .join("apps")
        .join("workflow-app")
        .join("production")
        .join("releases")
        .join("v1");
    write_js_workflow_scaffold(&release);
    let env_capture = temp.path().join("worker-env.txt");
    let worker_entry = release.join("node_modules/tako.sh/dist/entrypoints/bun-worker.mjs");
    std::fs::write(
        &worker_entry,
        format!(
            "cat <&3 >/dev/null\nprintf '%s\\n' \"$TAKO_BUILD|$CUSTOM_ENV|$TAKO_DATA_DIR|$TAKO_APP_NAME\" > {}\n",
            env_capture.display()
        ),
    )
    .unwrap();
    std::fs::write(
        release.join("app.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "runtime": "bun",
            "main": "index.js",
            "idle_timeout": 300,
            "env_vars": {
                "TAKO_BUILD": "v1",
                "CUSTOM_ENV": "worker-visible"
            }
        }))
        .unwrap(),
    )
    .unwrap();

    state
        .sync_app_workflows(app_id, &release, Some("/bin/sh"))
        .await;
    let supervisor = state
        .workflows
        .supervisor_for(app_id)
        .expect("release with workflows should register worker supervisor");
    supervisor.wake().unwrap();

    let captured = (0..50)
        .find_map(|_| {
            let value = std::fs::read_to_string(&env_capture).ok();
            if let Some(value) = value
                && !value.trim().is_empty()
            {
                return Some(value);
            }
            std::thread::sleep(Duration::from_millis(10));
            None
        })
        .expect("worker should record its environment");
    let expected_data_dir = temp
        .path()
        .join("apps")
        .join(app_id)
        .join("data")
        .join("app");
    assert_eq!(
        captured.trim(),
        format!(
            "v1|worker-visible|{}|workflow-app/production",
            expected_data_dir.display()
        )
    );
}

#[tokio::test]
async fn update_secrets_restarts_workflows_even_without_http_instances() {
    let temp = TempDir::new().unwrap();
    let app_id = "workflow-app/production";
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release_dir = temp
        .path()
        .join("apps")
        .join("workflow-app")
        .join("production")
        .join("releases")
        .join("v1");
    write_js_workflow_scaffold(&release_dir);
    write_release_manifest(
        &release_dir,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );

    let app = state.app_manager.register_app(AppConfig {
        name: "workflow-app".to_string(),
        environment: "production".to_string(),
        version: "v1".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "sleep 600".to_string(),
        ],
        min_instances: 0,
        max_instances: 4,
        idle_timeout: Duration::from_secs(300),
        ..Default::default()
    });
    state.load_balancer.register_app(app.clone());
    state.sync_app_workflows(app_id, &release_dir, None).await;

    let first = state
        .workflows
        .supervisor_for(app_id)
        .expect("initial workflow registration should succeed");
    let new_secrets: HashMap<String, String> = [("API_KEY".to_string(), "rotated".to_string())]
        .into_iter()
        .collect();

    let response = state
        .handle_command(Command::UpdateSecrets {
            app: app_id.to_string(),
            secrets: new_secrets.clone(),
        })
        .await;

    assert!(matches!(response, Response::Ok { .. }));
    let second = state
        .workflows
        .supervisor_for(app_id)
        .expect("workflow runtime should still be registered after secret rotation");
    assert!(
        !Arc::ptr_eq(&first, &second),
        "secret rotation should replace the workflow supervisor even with zero HTTP instances"
    );
    assert_eq!(state.state_store.get_secrets(app_id).unwrap(), new_secrets);
    assert_eq!(
        app.config.read().secrets.get("API_KEY"),
        Some(&"rotated".to_string())
    );
}

#[tokio::test]
async fn scale_command_persists_zero_instances_across_restore() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));

    let state_a = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager.clone(),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    let release_dir = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    std::fs::write(
        release_dir.join("app.json"),
        r#"{"runtime":"node","main":"index.js","idle_timeout":300,"start":["/bin/sh","-lc","sleep 600"]}"#,
    )
    .unwrap();

    let app = state_a.app_manager.register_app(AppConfig {
        name: "my-app".to_string(),
        version: "v1".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "sleep 600".to_string(),
        ],
        min_instances: 2,
        max_instances: 4,
        idle_timeout: Duration::from_secs(300),
        ..Default::default()
    });
    state_a.load_balancer.register_app(app.clone());
    {
        let mut route_table = state_a.routes.write().await;
        route_table.set_app_routes("my-app".to_string(), vec!["api.example.com".to_string()]);
    }

    let first = app.allocate_instance();
    first.set_state(InstanceState::Healthy);
    let second = app.allocate_instance();
    second.set_state(InstanceState::Healthy);

    let response = state_a
        .handle_command(Command::Scale {
            app: "my-app".to_string(),
            instances: 0,
        })
        .await;
    assert!(matches!(response, Response::Ok { .. }));
    assert_eq!(app.config.read().min_instances, 0);
    assert!(app.get_instances().is_empty());

    drop(state_a);

    let state_b = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    state_b.restore_from_state_store().await.unwrap();

    let restored = state_b.app_manager.get_app("my-app").expect("app restored");
    assert_eq!(restored.config.read().min_instances, 0);
    assert_eq!(restored.state(), AppState::Idle);
}

#[tokio::test]
async fn deploy_preserves_scaled_instance_count() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let current_release = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&current_release).unwrap();
    std::fs::write(
        current_release.join("app.json"),
        r#"{"runtime":"node","main":"index.js","idle_timeout":300,"start":["/bin/sh","-lc","sleep 600"]}"#,
    )
    .unwrap();

    let app = state.app_manager.register_app(AppConfig {
        name: "my-app".to_string(),
        version: "v1".to_string(),
        path: current_release.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "sleep 600".to_string(),
        ],
        min_instances: 2,
        max_instances: 4,
        idle_timeout: Duration::from_secs(300),
        ..Default::default()
    });
    state.load_balancer.register_app(app.clone());
    {
        let mut route_table = state.routes.write().await;
        route_table.set_app_routes("my-app".to_string(), vec!["api.example.com".to_string()]);
    }

    let old_instance = app.allocate_instance();
    old_instance.set_state(InstanceState::Healthy);

    let broken_release = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("releases")
        .join("v2");
    std::fs::create_dir_all(&broken_release).unwrap();
    std::fs::write(
        broken_release.join("app.json"),
        r#"{"runtime":"node","main":"index.js","idle_timeout":300,"start":["/bin/sh","-lc","exit 1"]}"#,
    )
    .unwrap();

    let response = state
        .handle_command(Command::Deploy {
            app: "my-app".to_string(),
            version: "v2".to_string(),
            path: broken_release.to_string_lossy().to_string(),
            routes: vec!["api.example.com".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            storages: Some(HashMap::new()),
            dns: None,
        })
        .await;

    assert!(matches!(response, Response::Error { .. }));
    assert_eq!(app.config.read().min_instances, 2);
}

#[tokio::test]
async fn delete_command_removes_persisted_state_for_next_boot() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state_a = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager.clone(),
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release_dir = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    write_release_manifest(
        &release_dir,
        "node",
        "index.js",
        &["/bin/sh", "-lc", "sleep 600"],
        Some("true"),
        300,
    );
    let app = state_a.app_manager.register_app(AppConfig {
        name: "my-app".to_string(),
        version: "v1".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "sleep 600".to_string(),
        ],
        min_instances: 0,
        ..Default::default()
    });
    state_a.load_balancer.register_app(app);
    {
        let mut route_table = state_a.routes.write().await;
        route_table.set_app_routes(
            "my-app/production".to_string(),
            vec!["api.example.com".to_string()],
        );
    }
    state_a.persist_app_state("my-app/production").await;

    let response = state_a
        .handle_command(Command::Delete {
            app: "my-app/production".to_string(),
        })
        .await;
    assert!(matches!(response, Response::Ok { .. }));

    let state_b = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();
    state_b.restore_from_state_store().await.unwrap();
    assert!(state_b.app_manager.get_app("my-app/production").is_none());
}

#[tokio::test]
async fn deploy_on_demand_validates_startup_and_fails_for_unhealthy_build() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release_dir = temp
        .path()
        .join("apps")
        .join("broken-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    std::fs::write(
        release_dir.join("app.json"),
        r#"{"runtime":"node","main":"index.js","idle_timeout":300,"install":"true","start":["/bin/sh","-lc","exit 1"]}"#,
    )
    .unwrap();

    let response = state
        .handle_command(Command::Deploy {
            app: "broken-app".to_string(),
            version: "v1".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            routes: vec!["broken.localhost".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            storages: Some(HashMap::new()),
            dns: None,
        })
        .await;

    assert!(
        matches!(response, Response::Error { .. }),
        "expected startup validation failure for on-demand deploy: {response:?}"
    );
}

// TODO: This test needs a rewrite to work with the plugin-derived launch
// command. The fake bun script exits immediately because the spawner's
// binary resolution doesn't find the fake bun via the manifest's PATH.
// The deploy lifecycle is fully covered by e2e tests (e2e/fixtures/).
#[tokio::test]
#[ignore = "needs rewrite for plugin architecture"]
async fn deploy_on_demand_keeps_one_warm_instance_after_successful_deploy() {
    if !python3_ok() || !python3_can_bind_loopback_tcp() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let runtime = ServerRuntimeConfig {
        socket: "/tmp/tako-warm.sock".to_string(),
        ..ServerRuntimeConfig::for_defaults(temp.path().to_path_buf())
    };
    let state = ServerState::new_with_runtime(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
        runtime,
    )
    .unwrap();

    let fake_bin_dir = temp.path().join("bin");
    std::fs::create_dir_all(&fake_bin_dir).unwrap();
    let fake_bun = fake_bin_dir.join("bun");
    let fake_server_py = temp.path().join("server.py");
    std::fs::write(
        &fake_server_py,
        r#"import json
import os
from http.server import BaseHTTPRequestHandler, HTTPServer

port = int(os.environ.get("PORT") or "0")
with os.fdopen(3, "r") as _bootstrap_fd:
    _bootstrap = json.load(_bootstrap_fd)
internal_token = _bootstrap.get("token") or ""
if not port or not internal_token:
raise SystemExit("PORT and fd 3 bootstrap token are required")

class Handler(BaseHTTPRequestHandler):
def do_GET(self):
    if self.path == "/status" and (self.headers.get("Host") or "").split(":")[0].lower() == "tako":
        if self.headers.get("X-Tako-Internal-Token") != internal_token:
            self.send_response(403)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"error":"forbidden"}')
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("X-Tako-Internal-Token", internal_token)
        self.end_headers()
        self.wfile.write(b'{"status":"ok"}')
        return
    self.send_response(404)
    self.end_headers()

def log_message(self, format, *args):
    return

HTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#,
    )
    .unwrap();
    std::fs::write(
        &fake_bun,
        format!(
            "#!/bin/sh\ncase \"$1\" in install) exit 0;; esac\nexec python3 {}\n",
            fake_server_py.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&fake_bun).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_bun, permissions).unwrap();
    }

    let release_dir = temp
        .path()
        .join("apps")
        .join("warm-app")
        .join("releases")
        .join("v1");
    std::fs::create_dir_all(&release_dir).unwrap();
    std::fs::write(
        release_dir.join("package.json"),
        r#"{"name":"warm-app","scripts":{"dev":"bun run index.ts"}}"#,
    )
    .unwrap();
    std::fs::write(release_dir.join("index.ts"), "export default {};\n").unwrap();
    std::fs::create_dir_all(release_dir.join("node_modules/tako.sh/dist/entrypoints")).unwrap();
    std::fs::write(
        release_dir.join("node_modules/tako.sh/dist/entrypoints/bun-server.mjs"),
        "export default {};",
    )
    .unwrap();
    // Include PATH in the manifest env_vars so that the spawned instance
    // can find the fake bun binary.  Also set runtime_bin to the absolute
    // path so resolve_runtime_binary picks it up directly.
    let path_with_fake = format!(
        "{}:{}",
        fake_bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    std::fs::write(
        release_dir.join("app.json"),
        serde_json::json!({
            "runtime": "bun",
            "main": "index.ts",
            "idle_timeout": 300,
            "env_vars": { "PATH": &path_with_fake }
        })
        .to_string(),
    )
    .unwrap();

    let app = state.app_manager.register_app(AppConfig {
        name: "warm-app".to_string(),
        version: "v0".to_string(),
        path: release_dir.clone(),
        command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "exit 0".to_string(),
        ],
        min_instances: 0,
        max_instances: 4,
        ..Default::default()
    });
    state.load_balancer.register_app(app);

    let response = state
        .handle_command(Command::Deploy {
            app: "warm-app".to_string(),
            version: "v1".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            routes: vec!["warm.localhost".to_string()],
            source_ip: tako_core::SourceIpMode::Auto,
            secrets: Some(HashMap::new()),
            storages: Some(HashMap::new()),
            dns: None,
        })
        .await;
    assert!(
        matches!(response, Response::Ok { .. }),
        "expected successful on-demand deploy: {response:?}"
    );

    let status = state
        .handle_command(Command::Status {
            app: "warm-app".to_string(),
        })
        .await;
    let Response::Ok { data } = status else {
        panic!("expected status response for warm-app");
    };

    assert_eq!(data.get("state").and_then(Value::as_str), Some("running"));
    let instances = data
        .get("instances")
        .and_then(Value::as_array)
        .expect("status should include instances");
    assert_eq!(instances.len(), 1);
}

#[tokio::test]
async fn instance_idle_event_resets_cold_start_when_app_scales_to_zero() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let app = state.app_manager.register_app(AppConfig {
        name: "idle-app".to_string(),
        version: "v1".to_string(),
        min_instances: 0,
        ..Default::default()
    });
    state.load_balancer.register_app(app.clone());
    app.set_state(AppState::Running);

    let instance = app.allocate_instance();
    instance.set_state(InstanceState::Healthy);

    // Simulate a prior successful cold start.
    state.cold_start.begin("idle-app");
    state.cold_start.mark_ready("idle-app");
    assert!(!state.cold_start.begin("idle-app").leader);

    handle_idle_event(
        &state,
        crate::scaling::IdleEvent::InstanceIdle {
            app: "idle-app".to_string(),
            instance_id: instance.id.clone(),
        },
    )
    .await;

    assert!(app.get_instances().is_empty());
    assert_eq!(app.state(), AppState::Idle);
    assert!(state.cold_start.begin("idle-app").leader);
}

#[tokio::test]
async fn instance_ready_event_sets_health_metric() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let app = state.app_manager.register_app(AppConfig {
        name: "metrics-app".to_string(),
        version: "v1".to_string(),
        min_instances: 1,
        ..Default::default()
    });
    state.load_balancer.register_app(app.clone());
    app.set_state(AppState::Running);

    let instance = app.allocate_instance();
    // Spawner sets state to Healthy directly before emitting Ready.
    instance.set_state(InstanceState::Healthy);

    handle_instance_event(
        &state,
        crate::instances::InstanceEvent::Ready {
            app: "metrics-app".to_string(),
            instance_id: instance.id.clone(),
        },
    )
    .await;

    let health = crate::metrics::INSTANCE_HEALTH
        .with_label_values(&[crate::metrics::server(), "metrics-app", &instance.id])
        .get();
    assert_eq!(
        health, 1,
        "InstanceEvent::Ready should set tako_instance_health to 1"
    );

    let running = crate::metrics::INSTANCES_RUNNING
        .with_label_values(&[crate::metrics::server(), "metrics-app"])
        .get();
    assert_eq!(
        running, 1,
        "InstanceEvent::Ready should update tako_instances_running"
    );
}

#[tokio::test]
async fn status_includes_running_builds_for_each_version() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let app = state.app_manager.register_app(AppConfig {
        name: "my-app".to_string(),
        version: "v1".to_string(),
        min_instances: 0,
        ..Default::default()
    });

    let old = app.allocate_instance();
    old.set_state(InstanceState::Healthy);

    let mut cfg = app.config.read().clone();
    cfg.version = "v2".to_string();
    app.update_config(cfg);

    let new = app.allocate_instance();
    new.set_state(InstanceState::Healthy);

    let response = state
        .handle_command(Command::Status {
            app: "my-app".to_string(),
        })
        .await;

    let Response::Ok { data } = response else {
        panic!("expected ok status response");
    };

    let builds = data
        .get("builds")
        .and_then(Value::as_array)
        .expect("status should include builds");
    let versions: Vec<&str> = builds
        .iter()
        .filter_map(|b| b.get("version").and_then(Value::as_str))
        .collect();
    assert!(
        versions.contains(&"v1") && versions.contains(&"v2"),
        "expected status to include both running builds: {data}"
    );
}

// CodeQL[rust/cleartext-logging]: hardcoded fixture secrets in tests; set_secrets encrypts at rest and update_secrets logs only app name.
#[tokio::test]
async fn run_release_executes_command_in_release_dir() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    // App name uses '/' separator; filesystem encodes this as two directory components.
    let release_dir = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("production")
        .join("releases")
        .join("abc1234");
    std::fs::create_dir_all(&release_dir).unwrap();
    let manifest = serde_json::json!({
        "runtime": "bun",
        "main": "index.ts",
        "idle_timeout": 300,
        "app_dir": "",
        "env_vars": {"NODE_ENV": "production"},
    });
    std::fs::write(
        release_dir.join("app.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    // Seed stale secrets for the app. The release command must use the
    // command payload, because deploy sends fresh secrets before Deploy stores
    // them in the state DB.
    state
        .state_store
        .set_secrets(
            "my-app/production",
            &HashMap::from([("DATABASE_URL".to_string(), "postgres://old".to_string())]),
        )
        .unwrap();

    let response = state
        .handle_command(Command::RunRelease {
            app: "my-app/production".to_string(),
            version: "abc1234".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            command_line: "printf %s \"$NODE_ENV-$DATABASE_URL\" > out.txt".to_string(),
            vars: HashMap::new(),
            secrets: HashMap::from([("DATABASE_URL".to_string(), "postgres://new".to_string())]),
        })
        .await;

    let data = match response {
        Response::Ok { data } => data,
        Response::Error { message } => panic!("expected ok, got: {message}"),
    };
    assert_eq!(data.get("exit_code").and_then(|v| v.as_i64()), Some(0));

    let written = std::fs::read_to_string(release_dir.join("out.txt")).unwrap();
    assert_eq!(written, "production-postgres://new");
}

// CodeQL[rust/cleartext-logging]: hardcoded fixture secrets in tests; set_secrets encrypts at rest and update_secrets logs only app name.
#[tokio::test]
async fn run_release_returns_error_on_nonzero_exit() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let release_dir = temp
        .path()
        .join("apps")
        .join("my-app")
        .join("production")
        .join("releases")
        .join("abc1234");
    std::fs::create_dir_all(&release_dir).unwrap();
    let manifest = serde_json::json!({
        "runtime": "bun",
        "main": "index.ts",
        "idle_timeout": 300,
        "app_dir": "",
    });
    std::fs::write(
        release_dir.join("app.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let response = state
        .handle_command(Command::RunRelease {
            app: "my-app/production".to_string(),
            version: "abc1234".to_string(),
            path: release_dir.to_string_lossy().to_string(),
            command_line: "echo boom 1>&2; exit 3".to_string(),
            vars: HashMap::new(),
            secrets: HashMap::new(),
        })
        .await;

    let Response::Error { message } = response else {
        panic!("expected error response, got: {response:?}");
    };
    assert!(
        message.contains('3'),
        "expected exit code in message: {message}"
    );
    assert!(
        message.contains("boom"),
        "expected stderr in message: {message}"
    );
}

#[tokio::test]
async fn run_release_rejects_path_outside_release_root() {
    let temp = TempDir::new().unwrap();
    let cert_manager = Arc::new(CertManager::new(CertManagerConfig {
        cert_dir: temp.path().join("certs"),
        ..Default::default()
    }));
    let state = ServerState::new(
        temp.path().to_path_buf(),
        cert_manager,
        None,
        empty_challenge_tokens(),
    )
    .unwrap();

    let response = state
        .handle_command(Command::RunRelease {
            app: "my-app/production".to_string(),
            version: "abc1234".to_string(),
            path: "/etc/passwd".to_string(),
            command_line: "true".to_string(),
            vars: HashMap::new(),
            secrets: HashMap::new(),
        })
        .await;

    assert!(matches!(response, Response::Error { .. }));
}

#[test]
fn read_server_config_from_json() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"server_name":"prod","trusted_proxy":{"proxy_protocol":true,"trusted_cidrs":["127.0.0.1/32"],"client_ip_headers":["cf-connecting-ip"]}}"#,
    )
    .unwrap();
    let config = read_server_config(dir.path());
    assert_eq!(config.server_name.as_deref(), Some("prod"));
    let trusted_proxy = config.trusted_proxy.unwrap();
    assert!(trusted_proxy.proxy_protocol);
    assert_eq!(trusted_proxy.trusted_cidrs, vec!["127.0.0.1/32"]);
    assert_eq!(trusted_proxy.client_ip_headers, vec!["cf-connecting-ip"]);
}

#[test]
fn read_server_config_returns_defaults_when_missing() {
    let dir = TempDir::new().unwrap();
    let config = read_server_config(dir.path());
    assert!(config.server_name.is_none());
}
