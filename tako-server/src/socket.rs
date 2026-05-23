//! Management socket for receiving commands from tako CLI
//!
//! Commands:
//! - deploy: Rolling update of an app
//! - stop: Stop an app
//! - delete: Delete an app from runtime state
//! - status: Get app status
//! - list: List all apps
//! - update_secrets: Update an app's secrets and apply by rolling restart
//! - server_info/enter_upgrading/exit_upgrading: Upgrade orchestration primitives

use std::future::Future;
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

use tako_socket::serve_jsonl_connection;

// Re-export protocol types from tako-core for shared use
pub use tako_core::{
    AppState, AppStatus, BuildStatus, Command, InstanceState, InstanceStatus, Response,
};

/// Management socket server.
///
/// Binds a pid-specific socket (`tako-{pid}.sock`) and atomically swaps a
/// stable symlink (the configured `path`) to point at it. This allows the new
/// server process to take over the symlink before the old one drains, giving
/// zero-downtime management socket handoff during reload.
pub struct SocketServer {
    /// The stable symlink path that CLI clients connect to (e.g. tako.sock)
    symlink_path: PathBuf,
    /// The pid-specific actual socket path (e.g. tako-12345.sock)
    actual_path: PathBuf,
}

impl Drop for SocketServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.actual_path);
    }
}

impl SocketServer {
    pub fn new(path: impl Into<String>) -> Self {
        let symlink_path = PathBuf::from(path.into());
        let dir = symlink_path
            .parent()
            .unwrap_or_else(|| Path::new("/var/run/tako"));
        let pid = std::process::id();
        let actual_path = dir.join(format!("tako-{pid}.sock"));
        Self {
            symlink_path,
            actual_path,
        }
    }

    /// The stable symlink path (used as the configured socket path by callers)
    /// Bind the pid-specific socket and atomically swap the stable symlink.
    ///
    /// This is intentionally **synchronous** so it can run before any async
    /// runtime work (ACME init, state restore, etc.), ensuring the new process
    /// takes over the management socket within milliseconds of starting.
    pub fn bind(&self) -> Result<std::os::unix::net::UnixListener, std::io::Error> {
        // Remove stale pid-specific socket file if present
        let _ = std::fs::remove_file(&self.actual_path);

        if let Some(parent) = self.actual_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let std_listener = std::os::unix::net::UnixListener::bind(&self.actual_path)?;
        std_listener.set_nonblocking(true)?;

        // Restrict socket to owner-only access (0600) so only the service user
        // can send management commands (deploy, secrets, etc.).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.actual_path, std::fs::Permissions::from_mode(0o600));
        }

        // Atomically swap symlink: write to a temp path then rename over the target.
        // rename(2) is atomic so clients see either the old or new target, never nothing.
        #[cfg(unix)]
        {
            let temp_link = self.symlink_path.with_extension("tmp");
            let _ = std::fs::remove_file(&temp_link);
            std::os::unix::fs::symlink(&self.actual_path, &temp_link)?;
            std::fs::rename(&temp_link, &self.symlink_path)?;
        }

        tracing::info!(
            actual = %self.actual_path.display(),
            symlink = %self.symlink_path.display(),
            "Management socket listening"
        );

        Ok(std_listener)
    }

    /// Run the accept loop on a pre-bound std listener, dispatching each
    /// connection to `handler`. Converts to tokio internally (must be called
    /// from within a Tokio runtime context).
    pub async fn serve<F, Fut>(
        std_listener: std::os::unix::net::UnixListener,
        handler: F,
    ) -> Result<(), std::io::Error>
    where
        F: Fn(Command) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Response> + Send + 'static,
    {
        let listener = UnixListener::from_std(std_listener)?;
        let handler = std::sync::Arc::new(handler);

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let handler = handler.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, handler).await {
                            tracing::error!("Connection error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Accept error: {}", e);
                }
            }
        }
    }

    /// Start listening for commands (convenience wrapper: bind + serve).
    #[cfg(test)]
    pub async fn run<F, Fut>(&self, handler: F) -> Result<(), std::io::Error>
    where
        F: Fn(Command) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Response> + Send + 'static,
    {
        let listener = self.bind()?;
        Self::serve(listener, handler).await
    }
}

async fn handle_connection<F, Fut>(
    stream: UnixStream,
    handler: std::sync::Arc<F>,
) -> Result<(), std::io::Error>
where
    F: Fn(Command) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    serve_jsonl_connection(
        stream,
        move |cmd| {
            let handler = handler.clone();
            async move {
                tracing::debug!("Received command: {:?}", cmd);
                handler(cmd).await
            }
        },
        |e| Response::error(format!("Invalid command: {}", e)),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileTypeExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::sleep;

    #[test]
    fn test_parse_prepare_release_command() {
        let json = r#"{"command": "prepare_release", "app": "my-app", "path": "/var/lib/tako/my-app/releases/1.0.0"}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();
        match cmd {
            Command::PrepareRelease { app, path } => {
                assert_eq!(app, "my-app");
                assert!(path.contains("releases"));
            }
            _ => panic!("Expected PrepareRelease command"),
        }
    }

    #[test]
    fn test_parse_deploy_command() {
        let json = r#"{"command": "deploy", "app": "my-app", "version": "1.0.0", "path": "/var/lib/tako/my-app/releases/1.0.0", "routes": ["api.example.com", "example.com/api/*"]}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();

        match cmd {
            Command::Deploy {
                app,
                version,
                path,
                routes,
                source_ip,
                secrets,
                storages,
                ssl,
            } => {
                assert_eq!(app, "my-app");
                assert_eq!(version, "1.0.0");
                assert!(path.contains("releases"));
                assert_eq!(routes.len(), 2);
                assert_eq!(source_ip, tako_core::SourceIpMode::Auto);
                assert!(secrets.is_none());
                assert!(storages.is_none());
                assert_eq!(ssl.provider, tako_core::SslProvider::LetsEncrypt);
            }
            _ => panic!("Expected Deploy command"),
        }
    }

    #[test]
    fn test_parse_scale_command() {
        let json = r#"{"command": "scale", "app": "my-app", "instances": 4}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();

        match cmd {
            Command::Scale { app, instances } => {
                assert_eq!(app, "my-app");
                assert_eq!(instances, 4);
            }
            _ => panic!("Expected Scale command"),
        }
    }

    #[test]
    fn test_parse_stop_command() {
        let json = r#"{"command": "stop", "app": "my-app"}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();

        match cmd {
            Command::Stop { app } => {
                assert_eq!(app, "my-app");
            }
            _ => panic!("Expected Stop command"),
        }
    }

    #[test]
    fn test_parse_list_command() {
        let json = r#"{"command": "list"}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();

        assert!(matches!(cmd, Command::List));
    }

    #[test]
    fn test_parse_delete_command() {
        let json = r#"{"command": "delete", "app": "my-app"}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();

        match cmd {
            Command::Delete { app } => assert_eq!(app, "my-app"),
            _ => panic!("Expected Delete command"),
        }
    }

    #[test]
    fn test_parse_hello_command() {
        let json = format!(
            r#"{{"command":"hello","protocol_version":{}}}"#,
            tako_core::PROTOCOL_VERSION
        );
        let cmd: Command = serde_json::from_str(&json).unwrap();
        match cmd {
            Command::Hello { protocol_version } => {
                assert_eq!(protocol_version, tako_core::PROTOCOL_VERSION)
            }
            _ => panic!("Expected Hello command"),
        }
    }

    #[test]
    fn test_parse_routes_command() {
        let json = r#"{"command": "routes"}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, Command::Routes));
    }

    #[test]
    fn test_parse_server_info_command() {
        let json = r#"{"command":"server_info"}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, Command::ServerInfo));
    }

    #[test]
    fn test_parse_list_releases_command() {
        let json = r#"{"command":"list_releases","app":"my-app"}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();
        match cmd {
            Command::ListReleases { app } => assert_eq!(app, "my-app"),
            _ => panic!("Expected ListReleases command"),
        }
    }

    #[test]
    fn test_parse_rollback_command() {
        let json = r#"{"command":"rollback","app":"my-app","version":"abc1234"}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();
        match cmd {
            Command::Rollback { app, version } => {
                assert_eq!(app, "my-app");
                assert_eq!(version, "abc1234");
            }
            _ => panic!("Expected Rollback command"),
        }
    }

    #[test]
    fn test_parse_enter_upgrading_command() {
        let json = r#"{"command":"enter_upgrading","owner":"controller-a"}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();
        match cmd {
            Command::EnterUpgrading { owner } => assert_eq!(owner, "controller-a"),
            _ => panic!("Expected EnterUpgrading command"),
        }
    }

    #[test]
    fn test_serialize_ok_response() {
        let response = Response::ok(serde_json::json!({"name": "my-app", "status": "running"}));
        let json = serde_json::to_string(&response).unwrap();

        assert!(json.contains("ok"));
        assert!(json.contains("my-app"));
    }

    #[test]
    fn test_serialize_error_response() {
        let response = Response::error("App not found");
        let json = serde_json::to_string(&response).unwrap();

        assert!(json.contains("error"));
        assert!(json.contains("App not found"));
    }

    #[test]
    fn test_app_state_serialization() {
        let state = AppState::Running;
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, r#""running""#);
    }

    #[test]
    fn test_instance_state_serialization() {
        let state = InstanceState::Healthy;
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, r#""healthy""#);
    }

    #[test]
    #[cfg(unix)]
    fn test_socket_server_new_sets_pid_specific_actual_path() {
        let temp = TempDir::new().unwrap();
        let symlink = temp.path().join("tako.sock");
        let server = SocketServer::new(symlink.to_string_lossy().to_string());
        let pid = std::process::id();
        let expected_actual = temp.path().join(format!("tako-{pid}.sock"));
        assert_eq!(server.actual_path, expected_actual);
        assert_eq!(server.symlink_path, symlink);
    }

    #[tokio::test]
    async fn test_handle_connection_returns_error_for_invalid_json() {
        let (mut client, server) = UnixStream::pair().unwrap();

        let handler = Arc::new(|_cmd: Command| async move { Response::ok(serde_json::json!({})) });
        let server_task = tokio::spawn(handle_connection(server, handler));

        client.write_all(b"not-json\n").await.unwrap();
        client.shutdown().await.unwrap();

        let mut raw = Vec::new();
        client.read_to_end(&mut raw).await.unwrap();
        let response = String::from_utf8(raw).unwrap();
        assert!(response.contains("\"status\":\"error\""), "{}", response);
        assert!(response.contains("Invalid command"), "{}", response);

        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_run_creates_pid_socket_and_symlink() {
        let temp = TempDir::new().unwrap();
        let probe_path = temp.path().join("probe.sock");
        if std::os::unix::net::UnixListener::bind(&probe_path).is_err() {
            return;
        }
        let _ = std::fs::remove_file(&probe_path);

        let symlink_path = temp.path().join("sockdir").join("tako.sock");
        std::fs::create_dir_all(symlink_path.parent().unwrap()).unwrap();
        // Write a stale file where the symlink will go — it must be atomically replaced
        std::fs::write(&symlink_path, b"stale-file").unwrap();

        let path_str = symlink_path.to_string_lossy().to_string();
        let server = SocketServer::new(path_str.clone());
        let server_task = tokio::spawn(async move {
            let _ = server
                .run(|cmd| async move {
                    match cmd {
                        Command::List => Response::ok(serde_json::json!({"ok": true})),
                        _ => Response::error("unexpected command"),
                    }
                })
                .await;
        });

        // Wait until the symlink resolves to a connectable socket
        let mut ready = false;
        for _ in 0..100 {
            if let Ok(meta) = std::fs::metadata(&symlink_path)
                && meta.file_type().is_socket()
            {
                ready = true;
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        assert!(
            ready,
            "socket was not reachable via symlink {}",
            symlink_path.display()
        );

        // Confirm the stable path is now a symlink (not a plain socket file)
        assert!(
            std::fs::symlink_metadata(&symlink_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "expected symlink at {}",
            symlink_path.display()
        );

        let mut client = UnixStream::connect(&path_str).await.unwrap();
        client.write_all(br#"{"command":"list"}"#).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        client.shutdown().await.unwrap();

        let mut raw = Vec::new();
        client.read_to_end(&mut raw).await.unwrap();
        let response = String::from_utf8(raw).unwrap();
        assert!(response.contains("\"status\":\"ok\""), "{}", response);

        server_task.abort();
        let _ = server_task.await;
    }
}
