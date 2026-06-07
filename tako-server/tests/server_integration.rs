//! Server Integration Tests
//!
//! Tests the tako-server functionality including:
//! - Instance management
//! - Reload command handling
//! - Health endpoint availability

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn workspace_root() -> PathBuf {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| manifest_dir.to_path_buf())
}

fn apply_coverage_env(cmd: &mut Command) {
    let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") else {
        return;
    };
    let profile = PathBuf::from(profile);
    if profile.is_absolute() {
        return;
    }
    let absolute = workspace_root().join(profile);
    if let Some(parent) = absolute.parent() {
        let _ = fs::create_dir_all(parent);
    }
    cmd.env("LLVM_PROFILE_FILE", absolute);
}

fn pick_unused_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("failed to bind to ephemeral port")
        .local_addr()
        .expect("failed to read local addr")
        .port()
}

fn can_bind_localhost() -> bool {
    TcpListener::bind("127.0.0.1:0").is_ok()
}

fn should_fail_when_localhost_bind_unavailable(ci_env: Option<&str>) -> bool {
    ci_env.is_some_and(|value| !value.trim().is_empty())
}

fn require_localhost_bind() -> bool {
    if can_bind_localhost() {
        return true;
    }
    if should_fail_when_localhost_bind_unavailable(std::env::var("CI").ok().as_deref()) {
        panic!("integration test requires localhost bind access in CI environment");
    }
    eprintln!("skipping integration test: localhost bind access unavailable");
    false
}

fn bun_available() -> bool {
    Command::new("bun")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn e2e_enabled() -> bool {
    std::env::var("TAKO_E2E").is_ok()
}

/// Helper to start tako-server in background
struct TestServer {
    child: Option<Child>,
    socket_path: PathBuf,
    data_dir: TempDir,
    http_port: u16,
    tls_port: u16,
}

const SERVER_START_RETRIES: usize = 5;
const SERVER_START_POLL_ATTEMPTS: usize = 100;
const SERVER_START_POLL_DELAY: Duration = Duration::from_millis(100);
const SERVER_START_RETRY_DELAY: Duration = Duration::from_millis(50);

impl TestServer {
    fn start() -> Self {
        let data_dir = TempDir::new().unwrap();
        let socket_path = data_dir.path().join("tako.sock");
        let mut last_error = None;

        for attempt in 1..=SERVER_START_RETRIES {
            let http_port = pick_unused_port();
            let tls_port = pick_unused_port();

            let _ = fs::remove_file(&socket_path);
            let mut child = spawn_test_server(&socket_path, data_dir.path(), http_port, tls_port);
            match wait_for_server_socket(&socket_path, &mut child)
                .and_then(|()| wait_for_server_http(http_port, &mut child))
            {
                Ok(()) => {
                    return TestServer {
                        child: Some(child),
                        socket_path,
                        data_dir,
                        http_port,
                        tls_port,
                    };
                }
                Err(error) => {
                    last_error = Some(format!(
                        "attempt {attempt}/{SERVER_START_RETRIES} failed (http={http_port}, tls={tls_port}): {error}"
                    ));
                    let _ = child.kill();
                    let _ = child.wait();
                    thread::sleep(SERVER_START_RETRY_DELAY);
                }
            }
        }

        panic!(
            "failed to start tako-server after {} attempts: {}",
            SERVER_START_RETRIES,
            last_error.unwrap_or_else(|| "unknown error".to_string())
        );
    }

    fn send_command(&self, command: &serde_json::Value) -> serde_json::Value {
        let mut stream =
            UnixStream::connect(&self.socket_path).expect("Failed to connect to server socket");

        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        writeln!(stream, "{}", command).expect("Failed to send command");

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .expect("Failed to read response");

        serde_json::from_str(&response).unwrap_or_else(|_| {
            serde_json::json!({
                "status": "error",
                "message": format!("Invalid JSON response: {}", response.trim()),
            })
        })
    }

    fn http_get(&self, path: &str) -> Result<String, String> {
        self.http_get_with_host("localhost", path)
    }

    fn http_get_with_host(&self, host: &str, path: &str) -> Result<String, String> {
        self.http_get_with_host_and_headers(host, path, &[])
    }

    fn http_get_with_host_and_headers(
        &self,
        host: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Result<String, String> {
        let addr = format!("127.0.0.1:{}", self.http_port);
        let mut stream =
            TcpStream::connect(&addr).map_err(|e| format!("Failed to connect: {}", e))?;

        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let extra_headers = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect::<String>();
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\n{}Connection: close\r\n\r\n",
            path, host, extra_headers
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|e| format!("Failed to write: {}", e))?;

        let mut response = Vec::new();
        std::io::Read::read_to_end(&mut stream, &mut response)
            .map_err(|e| format!("Failed to read: {}", e))?;

        String::from_utf8(response).map_err(|e| format!("Invalid UTF-8: {}", e))
    }

    fn data_dir(&self) -> &std::path::Path {
        self.data_dir.path()
    }

    fn https_status_with_host(&self, host: &str, path: &str) -> Result<u16, String> {
        let url = format!("https://{}:{}{}", host, self.tls_port, path);
        let resolve = std::net::SocketAddr::from(([127, 0, 0, 1], self.tls_port));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("runtime build failed: {e}"))?;

        runtime.block_on(async move {
            let client = reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .resolve(host, resolve)
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| format!("https client error: {e}"))?;

            let response = client
                .get(url)
                .send()
                .await
                .map_err(|e| format!("https request error: {e}"))?;

            Ok(response.status().as_u16())
        })
    }
}

fn spawn_test_server(
    socket_path: &std::path::Path,
    data_dir: &std::path::Path,
    http_port: u16,
    tls_port: u16,
) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tako-server"));
    cmd.arg("--socket")
        .arg(socket_path)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--http-port")
        .arg(http_port.to_string())
        .arg("--https-port")
        .arg(tls_port.to_string())
        .arg("--no-acme")
        .arg("--metrics-port")
        .arg("0")
        .env("RUST_LOG", "warn")
        .env("TAKO_TEST_SKIP_CLOUDFLARE_IP_REFRESH", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    apply_coverage_env(&mut cmd);
    cmd.spawn().expect("Failed to start tako-server")
}

fn wait_for_server_socket(socket_path: &std::path::Path, child: &mut Child) -> Result<(), String> {
    for _ in 0..SERVER_START_POLL_ATTEMPTS {
        if socket_path.exists() && UnixStream::connect(socket_path).is_ok() {
            thread::sleep(SERVER_START_POLL_DELAY);
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "tako-server exited before socket became available: {status}"
            ));
        }
        thread::sleep(SERVER_START_POLL_DELAY);
    }
    Err("server socket never became available".to_string())
}

fn wait_for_server_http(http_port: u16, child: &mut Child) -> Result<(), String> {
    for _ in 0..SERVER_START_POLL_ATTEMPTS {
        if TcpStream::connect(("127.0.0.1", http_port)).is_ok() {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "tako-server exited before HTTP became available: {status}"
            ));
        }
        thread::sleep(SERVER_START_POLL_DELAY);
    }
    Err("server HTTP port never became available".to_string())
}

fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(100));
    }
    None
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[path = "server_integration/channels.rs"]
mod channels;
#[path = "server_integration/health_check.rs"]
mod health_check;
#[path = "server_integration/instance_management.rs"]
mod instance_management;
#[path = "server_integration/localhost_bind.rs"]
mod localhost_bind;
#[path = "server_integration/protocol.rs"]
mod protocol;
#[path = "server_integration/server_info.rs"]
mod server_info;
