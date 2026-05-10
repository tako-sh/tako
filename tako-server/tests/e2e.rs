//! End-to-End Tests
//!
//! Full integration tests that deploy real apps and verify:
//! - socket protocol error handling
//! - deploy flow (when `TAKO_E2E=1` and Bun is available)
//! - host/path routing (when `TAKO_E2E=1` and Bun is available)
//! - rolling deploy path (when `TAKO_E2E=1` and Bun is available)

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
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

fn pick_unused_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map(|addr| addr.port())
}

fn pick_unused_port_pair() -> Option<(u16, u16)> {
    for _ in 0..32 {
        let Some(http_port) = pick_unused_port() else {
            continue;
        };
        let Some(https_port) = pick_unused_port() else {
            continue;
        };
        if http_port != https_port {
            return Some((http_port, https_port));
        }
    }
    None
}

fn can_bind_localhost() -> bool {
    TcpListener::bind("127.0.0.1:0").is_ok()
}

fn bun_available() -> bool {
    Command::new("bun")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn e2e_enabled() -> bool {
    std::env::var("TAKO_E2E").is_ok() && bun_available() && can_bind_localhost()
}

fn e2e_environment_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn format_output_field(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "(empty)".to_string()
    } else {
        trimmed.to_string()
    }
}

fn bun_app_source(body: &str) -> String {
    format!(
        r#"import {{ closeSync, readFileSync }} from "node:fs";

const port = Number(process.env.PORT ?? "3000");
const host = process.env.HOST ?? "127.0.0.1";
const bootstrap = JSON.parse(readFileSync(3, "utf-8"));
closeSync(3);
const internalToken = bootstrap.token;
if (!internalToken) {{
  throw new Error("bootstrap envelope on fd 3 did not provide a token");
}}
const internalAppName = (process.env.TAKO_APP_NAME ?? "app").split("/")[0] || "app";
const internalHost = `${{internalAppName}}.tako`;

Bun.serve({{
  hostname: host,
  port,
  fetch(request) {{
    const url = new URL(request.url);
    const path = url.pathname;
    const requestHost = (request.headers.get("host") ?? url.host).split(":")[0]?.toLowerCase();
    if (requestHost === internalHost && path === "/status") {{
      if (request.headers.get("x-tako-internal-token") !== internalToken) {{
        return new Response(JSON.stringify({{ error: "forbidden" }}), {{
          status: 403,
          headers: {{ "Content-Type": "application/json" }},
        }});
      }}
      return new Response(JSON.stringify({{ status: "ok" }}), {{
        headers: {{
          "Content-Type": "application/json",
          "X-Tako-Internal-Token": internalToken,
        }},
      }});
    }}
    return new Response({body:?});
  }},
}});
"#
    )
}

/// E2E test environment with tako-server running.
struct E2EEnvironment {
    server_process: Option<Child>,
    _test_lock: MutexGuard<'static, ()>,
    server_socket: PathBuf,
    http_port: u16,
    data_dir: TempDir,
}

impl E2EEnvironment {
    fn new() -> Self {
        let test_lock = e2e_environment_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let data_dir = TempDir::new().unwrap();
        let server_socket = data_dir.path().join("tako.sock");

        let (http_port, https_port) = pick_unused_port_pair().unwrap_or((18080, 18443));

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tako-server"));
        cmd.arg("--socket")
            .arg(&server_socket)
            .arg("--data-dir")
            .arg(data_dir.path())
            .arg("--port")
            .arg(http_port.to_string())
            .arg("--tls-port")
            .arg(https_port.to_string())
            .arg("--no-acme")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_coverage_env(&mut cmd);
        let server_process = cmd.spawn().expect("Failed to start tako-server");

        let mut env = E2EEnvironment {
            server_process: Some(server_process),
            _test_lock: test_lock,
            server_socket,
            http_port,
            data_dir,
        };

        env.wait_for_ready();
        env
    }

    fn wait_for_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self.server_socket.exists() {
                thread::sleep(Duration::from_millis(200));
                return;
            }

            if let Some(status) = self
                .server_process
                .as_mut()
                .and_then(|child| child.try_wait().ok())
                .flatten()
            {
                self.panic_with_server_output(&format!(
                    "Server exited before becoming ready (status: {})",
                    status
                ));
            }
            thread::sleep(Duration::from_millis(100));
        }

        self.panic_with_server_output("Server did not become ready in time");
    }

    fn panic_with_server_output(&mut self, reason: &str) -> ! {
        let details = if let Some(mut child) = self.server_process.take() {
            let _ = child.kill();
            match child.wait_with_output() {
                Ok(output) => format!(
                    "tako-server output\nstdout:\n{}\n\nstderr:\n{}",
                    format_output_field(&output.stdout),
                    format_output_field(&output.stderr)
                ),
                Err(err) => format!("failed to read tako-server output: {}", err),
            }
        } else {
            "tako-server output unavailable: process handle already consumed".to_string()
        };
        panic!("{}\n{}", reason, details);
    }

    fn send_command(&self, command: &serde_json::Value) -> serde_json::Value {
        let mut stream =
            UnixStream::connect(&self.server_socket).expect("Failed to connect to server");

        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();

        writeln!(stream, "{}", command).expect("Failed to write command");

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .expect("Failed to read response");

        serde_json::from_str(&response).unwrap_or_else(
            |_| serde_json::json!({"status": "error", "message": "Failed to parse response", "raw": response}),
        )
    }

    fn send_raw_command(&self, line: &str) -> String {
        let mut stream =
            UnixStream::connect(&self.server_socket).expect("Failed to connect to server");

        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();

        writeln!(stream, "{}", line).expect("Failed to write command");

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .expect("Failed to read response");
        response
    }

    fn create_test_app(&self, app: &str, version: &str, code: &str) -> PathBuf {
        let app_dir = self
            .data_dir
            .path()
            .join("apps")
            .join(app)
            .join("production")
            .join("releases")
            .join(version);
        fs::create_dir_all(&app_dir).unwrap();
        fs::create_dir_all(app_dir.join("node_modules/tako.sh/dist/entrypoints")).unwrap();

        fs::write(
            app_dir.join("package.json"),
            format!(
                r#"{{"name":"{}","scripts":{{"dev":"bun run index.ts"}}}}"#,
                app
            ),
        )
        .unwrap();
        fs::write(
            app_dir.join("node_modules/tako.sh/dist/entrypoints/bun-server.mjs"),
            "await import(process.argv[2]);",
        )
        .unwrap();
        fs::write(
            app_dir.join("app.json"),
            r#"{"runtime":"bun","main":"index.ts","idle_timeout":300,"install":"true","start":["bun","{main}"]}"#,
        )
        .unwrap();
        fs::write(app_dir.join("index.ts"), code).unwrap();
        app_dir
    }

    fn http_get_with_host(&self, path: &str, host: &str) -> Result<String, String> {
        let addr = format!("127.0.0.1:{}", self.http_port);
        let mut stream =
            TcpStream::connect(&addr).map_err(|e| format!("Failed to connect: {}", e))?;

        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            path, host
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|e| format!("Failed to write: {}", e))?;

        let mut response = Vec::new();
        std::io::Read::read_to_end(&mut stream, &mut response)
            .map_err(|e| format!("Failed to read: {}", e))?;

        String::from_utf8(response).map_err(|e| format!("Invalid UTF-8: {}", e))
    }
}

impl Drop for E2EEnvironment {
    fn drop(&mut self) {
        if let Some(mut child) = self.server_process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn deploy_command(
    app: &str,
    version: &str,
    path: &std::path::Path,
    routes: &[&str],
    _instances: u8,
) -> serde_json::Value {
    serde_json::json!({
        "command": "deploy",
        "app": tako_core::deployment_app_id(app, "production"),
        "version": version,
        "path": path.to_string_lossy(),
        "routes": routes,
    })
}

mod harness {
    use super::*;

    #[test]
    fn pick_unused_port_pair_returns_distinct_ports() {
        if !can_bind_localhost() {
            return;
        }

        let (http_port, https_port) =
            pick_unused_port_pair().expect("should allocate a pair of localhost ports");
        assert_ne!(
            http_port, https_port,
            "http and https ports must never be identical"
        );
    }
}

mod deploy_flow {
    use super::*;

    #[test]
    fn test_init_build_deploy_request() {
        if !e2e_enabled() {
            return;
        }

        let env = E2EEnvironment::new();
        let app_source = bun_app_source("Hello from Tako!");

        let app_dir = env.create_test_app("hello-world", "v1", &app_source);

        let response = env.send_command(&deploy_command(
            "hello-world",
            "v1",
            &app_dir,
            &["hello-world.localhost"],
            1,
        ));

        assert_eq!(response.get("status").and_then(|s| s.as_str()), Some("ok"));

        thread::sleep(Duration::from_secs(2));

        let response = env
            .http_get_with_host("/", "hello-world.localhost")
            .expect("request should succeed");
        assert!(
            response.contains("Hello from Tako!"),
            "expected app response, got: {}",
            response
        );
    }
}

mod routing {
    use super::*;

    #[test]
    fn test_multiple_apps_routing() {
        if !e2e_enabled() {
            return;
        }

        let env = E2EEnvironment::new();
        let app_a_source = bun_app_source("App A");
        let app_b_source = bun_app_source("App B");

        let app_a = env.create_test_app("app-a", "v1", &app_a_source);
        let app_b = env.create_test_app("app-b", "v1", &app_b_source);

        let deploy_a = env.send_command(&deploy_command(
            "app-a",
            "v1",
            &app_a,
            &["router.localhost/a/*"],
            1,
        ));
        assert_eq!(deploy_a.get("status").and_then(|s| s.as_str()), Some("ok"));

        let deploy_b = env.send_command(&deploy_command(
            "app-b",
            "v1",
            &app_b,
            &["router.localhost/b/*"],
            1,
        ));
        assert_eq!(deploy_b.get("status").and_then(|s| s.as_str()), Some("ok"));

        thread::sleep(Duration::from_secs(2));

        let a_response = env
            .http_get_with_host("/a/one", "router.localhost")
            .expect("app-a request should succeed");
        assert!(
            a_response.contains("App A"),
            "expected App A response, got: {}",
            a_response
        );

        let b_response = env
            .http_get_with_host("/b/two", "router.localhost")
            .expect("app-b request should succeed");
        assert!(
            b_response.contains("App B"),
            "expected App B response, got: {}",
            b_response
        );
    }
}

mod rolling_updates {
    use super::*;

    #[test]
    fn test_rolling_update_deploys_new_version() {
        if !e2e_enabled() {
            return;
        }

        let env = E2EEnvironment::new();
        let v1_source = bun_app_source("v1");

        let v1_dir = env.create_test_app("versioned-app", "v1", &v1_source);

        let deploy_v1 = env.send_command(&deploy_command(
            "versioned-app",
            "v1",
            &v1_dir,
            &["rolling.localhost"],
            2,
        ));
        assert_eq!(deploy_v1.get("status").and_then(|s| s.as_str()), Some("ok"));

        thread::sleep(Duration::from_secs(2));
        let before = env
            .http_get_with_host("/", "rolling.localhost")
            .expect("v1 request should succeed");
        assert!(
            before.contains("v1"),
            "expected v1 response, got: {}",
            before
        );

        let v2_source = bun_app_source("v2");
        let v2_dir = env.create_test_app("versioned-app", "v2", &v2_source);

        let deploy_v2 = env.send_command(&deploy_command(
            "versioned-app",
            "v2",
            &v2_dir,
            &["rolling.localhost"],
            2,
        ));
        assert_eq!(deploy_v2.get("status").and_then(|s| s.as_str()), Some("ok"));

        thread::sleep(Duration::from_secs(2));
        let after = env
            .http_get_with_host("/", "rolling.localhost")
            .expect("v2 request should succeed");
        assert!(after.contains("v2"), "expected v2 response, got: {}", after);
    }
}

mod reload {
    use super::*;

    /// HTTP GET with X-Forwarded-Proto: https to bypass HTTP→HTTPS redirect.
    fn http_get_as_https(http_port: u16, path: &str, host: &str) -> Result<String, String> {
        let addr = format!("127.0.0.1:{http_port}");
        let mut stream =
            TcpStream::connect(&addr).map_err(|e| format!("Failed to connect: {e}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nX-Forwarded-Proto: https\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|e| format!("Failed to write: {e}"))?;
        let mut response = Vec::new();
        std::io::Read::read_to_end(&mut stream, &mut response)
            .map_err(|e| format!("Failed to read: {e}"))?;
        String::from_utf8(response).map_err(|e| format!("Invalid UTF-8: {e}"))
    }

    #[test]
    fn test_reload_preserves_deployed_apps() {
        if !e2e_enabled() {
            return;
        }

        let env = E2EEnvironment::new();
        let app_source = bun_app_source("reload-test-v1");

        // Deploy an app
        let app_dir = env.create_test_app("reload-test", "v1", &app_source);

        let resp = env.send_command(&deploy_command(
            "reload-test",
            "v1",
            &app_dir,
            &["reload-test.localhost"],
            1,
        ));
        assert_eq!(resp.get("status").and_then(|s| s.as_str()), Some("ok"));

        // Wait for app to be ready
        thread::sleep(Duration::from_secs(2));

        // Verify app is serving (use X-Forwarded-Proto to bypass HTTP→HTTPS redirect)
        let body = http_get_as_https(env.http_port, "/", "reload-test.localhost")
            .expect("app should respond before reload");
        assert!(
            body.contains("reload-test-v1"),
            "expected app response before reload, got: {body}"
        );

        // Get old PID
        let info = env.send_command(&serde_json::json!({ "command": "server_info" }));
        let old_pid = info
            .get("data")
            .and_then(|d| d.get("pid"))
            .and_then(|p| p.as_u64())
            .expect("server_info should include pid") as u32;

        // Send SIGHUP to trigger reload
        unsafe {
            libc::kill(old_pid as i32, libc::SIGHUP);
        }

        // Wait for new process to take over
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut new_pid = None;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(300));
            let resp = env.send_command(&serde_json::json!({ "command": "server_info" }));
            if let Some(pid) = resp
                .get("data")
                .and_then(|d| d.get("pid"))
                .and_then(|p| p.as_u64())
                && pid as u32 != old_pid
            {
                new_pid = Some(pid as u32);
                break;
            }
        }
        let new_pid = new_pid.expect("new server should take over after SIGHUP");

        // Verify app is still listed
        let list_resp = env.send_command(&serde_json::json!({ "command": "list" }));
        assert_eq!(
            list_resp.get("status").and_then(|s| s.as_str()),
            Some("ok"),
            "list should succeed: {list_resp}"
        );
        let apps = list_resp
            .get("data")
            .and_then(|d| d.get("apps"))
            .and_then(|a| a.as_array())
            .expect("list should return data.apps array");
        let has_app = apps
            .iter()
            .any(|a| a.get("name").and_then(|n| n.as_str()) == Some("reload-test"));
        assert!(has_app, "reload-test app should still be listed: {apps:?}");

        // Wait for app instances to be healthy under new process
        thread::sleep(Duration::from_secs(2));

        // Verify app still serves traffic
        let body = http_get_as_https(env.http_port, "/", "reload-test.localhost")
            .expect("app should respond after reload");
        assert!(
            body.contains("reload-test-v1"),
            "expected app response after reload, got: {body}"
        );

        // Clean up new process
        unsafe {
            libc::kill(new_pid as i32, libc::SIGTERM);
        }
    }
}

mod error_handling {
    use super::*;

    #[test]
    fn test_invalid_command_handling() {
        if !can_bind_localhost() {
            return;
        }

        let env = E2EEnvironment::new();
        let response = env.send_raw_command("not json at all");

        let parsed: serde_json::Value =
            serde_json::from_str(&response).expect("response should be valid JSON");
        assert_eq!(parsed.get("status").and_then(|s| s.as_str()), Some("error"));
    }

    #[test]
    fn test_unknown_command_type() {
        if !can_bind_localhost() {
            return;
        }

        let env = E2EEnvironment::new();
        let response = env.send_command(&serde_json::json!({
            "command": "unknown_command_xyz",
            "data": "test"
        }));

        assert_eq!(
            response.get("status").and_then(|s| s.as_str()),
            Some("error")
        );
    }
}

mod acme_challenge {
    use super::*;

    /// Verify that injected ACME challenge tokens are served via HTTP.
    /// This test catches the bug where challenge responses were stored in ctx
    /// but never written to the HTTP connection (fixed in ceabba1).
    #[test]
    fn test_acme_challenge_token_served_via_http() {
        if !can_bind_localhost() {
            return;
        }

        let env = E2EEnvironment::new();

        // Inject a challenge token via socket
        let response = env.send_command(&serde_json::json!({
            "command": "inject_challenge_token",
            "token": "test-token-abc123",
            "key_authorization": "test-token-abc123.thumbprint-xyz"
        }));
        assert_eq!(
            response.get("status").and_then(|s| s.as_str()),
            Some("ok"),
            "inject_challenge_token should succeed: {:?}",
            response
        );

        // Make HTTP request to the challenge path
        let http_response = env
            .http_get_with_host(
                "/.well-known/acme-challenge/test-token-abc123",
                "any-domain.example.com",
            )
            .expect("HTTP request should succeed");

        // Verify 200 response with correct key authorization
        assert!(
            http_response.contains("200"),
            "Expected HTTP 200, got: {}",
            http_response.lines().next().unwrap_or("")
        );
        assert!(
            http_response.contains("test-token-abc123.thumbprint-xyz"),
            "Response body should contain key authorization, got: {}",
            http_response
        );
    }

    /// Verify that unknown challenge tokens return 404.
    #[test]
    fn test_acme_challenge_unknown_token_returns_404() {
        if !can_bind_localhost() {
            return;
        }

        let env = E2EEnvironment::new();

        let http_response = env
            .http_get_with_host(
                "/.well-known/acme-challenge/nonexistent-token",
                "any-domain.example.com",
            )
            .expect("HTTP request should succeed");

        assert!(
            http_response.contains("404"),
            "Expected HTTP 404 for unknown token, got: {}",
            http_response.lines().next().unwrap_or("")
        );
        assert!(
            http_response.contains("Token not found"),
            "Response should say 'Token not found', got: {}",
            http_response
        );
    }
}
