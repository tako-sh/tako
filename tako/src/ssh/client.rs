//! SSH client implementation using russh

mod auth;
mod exec;
mod filesystem;
mod tako;

pub use tako::{InstallServerMode, ServerInstallPorts};

use super::error::{SshError, SshResult};
use russh::Disconnect;
use russh::client::{self, Config, Handle, Handler};
use russh::keys::{PublicKey, check_known_hosts_path};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tako_core::Response;

/// Truncate a remote command for logging: show only the first line (up to 120 chars).
const TAKO_SERVER_SERVICE_HELPER: &str = "/usr/local/bin/tako-server-service";

/// SSH connection configuration
#[derive(Clone)]
pub struct SshConfig {
    /// Remote hostname or IP
    pub host: String,
    /// Remote SSH user
    pub user: String,
    /// SSH port (default 22)
    pub port: u16,
    /// Connection timeout
    pub timeout: Duration,
    /// Path to SSH keys directory (default ~/.ssh)
    pub keys_dir: Option<PathBuf>,
    /// Passphrase for encrypted local SSH private keys.
    pub key_passphrase: Option<String>,
}

impl std::fmt::Debug for SshConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshConfig")
            .field("host", &self.host)
            .field("user", &self.user)
            .field("port", &self.port)
            .field("timeout", &self.timeout)
            .field("keys_dir", &self.keys_dir)
            .field(
                "key_passphrase",
                &self.key_passphrase.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl SshConfig {
    /// Create config from server entry
    pub fn from_server(host: &str, port: u16) -> Self {
        Self::for_user(host, port, "tako")
    }

    /// Create config for a specific SSH user.
    pub fn for_user(host: &str, port: u16, user: &str) -> Self {
        Self {
            host: host.to_string(),
            user: user.to_string(),
            port,
            timeout: Duration::from_secs(30),
            keys_dir: None,
            key_passphrase: super::configured_key_passphrase(),
        }
    }

    /// Get the SSH keys directory
    pub fn keys_directory(&self) -> PathBuf {
        self.keys_dir.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".ssh")
        })
    }

    /// Get the known_hosts file path.
    pub fn known_hosts_file(&self) -> PathBuf {
        self.keys_directory().join("known_hosts")
    }
}

/// Output from a command execution
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Exit code (0 = success)
    pub exit_code: u32,
    /// Standard output
    pub stdout: String,
    /// Standard error
    pub stderr: String,
}

impl CommandOutput {
    /// Check if command succeeded
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }

    /// Get combined output (stdout + stderr)
    pub fn combined(&self) -> String {
        if self.stderr.is_empty() {
            self.stdout.clone()
        } else if self.stdout.is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n{}", self.stdout, self.stderr)
        }
    }
}

/// Handler for SSH client events
pub struct SshHandler {
    /// Expected host name
    host: String,
    /// Expected host port
    port: u16,
    /// Path to known_hosts file
    known_hosts_path: PathBuf,
}

impl Handler for SshHandler {
    type Error = SshError;

    fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> impl Future<Output = std::result::Result<bool, Self::Error>> + Send {
        let host = self.host.clone();
        let port = self.port;
        let known_hosts_path = self.known_hosts_path.clone();
        let known_hosts_display = known_hosts_path.display().to_string();
        let verification =
            check_known_hosts_path(&host, port, server_public_key, &known_hosts_path)
                .map_err(|error| error.to_string());

        async move {
            match verification {
                Ok(true) => Ok(true),
                Ok(false) => Err(SshError::HostKeyVerification {
                    host: format!("{host}:{port} (not found in {known_hosts_display})"),
                }),
                Err(error) => Err(SshError::HostKeyVerification {
                    host: format!("{host}:{port} ({error})"),
                }),
            }
        }
    }
}

/// SSH client for remote operations
pub struct SshClient {
    config: SshConfig,
    /// SSH session handle (public for SFTP access)
    pub handle: Option<Handle<SshHandler>>,
    authenticated_public_key: Option<String>,

    tako_hello_checked: std::sync::atomic::AtomicBool,
}

impl SshClient {
    /// Create a new SSH client
    pub fn new(config: SshConfig) -> Self {
        Self {
            config,
            handle: None,
            authenticated_public_key: None,
            tako_hello_checked: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Create and connect to a server in one step.
    pub async fn connect_to(host: &str, port: u16) -> SshResult<Self> {
        let mut client = Self::new(SshConfig::from_server(host, port));
        client.connect().await?;
        Ok(client)
    }

    fn interpret_hello_response(resp: &Response) -> Result<(), String> {
        match resp {
            Response::Ok { .. } => Ok(()),
            Response::Error { message } => {
                if message.to_lowercase().contains("protocol version mismatch") {
                    return Err(format!("Remote tako-server protocol mismatch: {message}"));
                }
                Err(format!("tako-server handshake failed: {message}"))
            }
        }
    }

    /// Connect to the remote server
    pub async fn connect(&mut self) -> SshResult<()> {
        let _t = crate::output::timed(&format!(
            "SSH connect to {}@{}:{}",
            self.config.user, self.config.host, self.config.port
        ));
        let ssh_config = Config {
            inactivity_timeout: Some(self.config.timeout),
            keepalive_interval: Some(Duration::from_secs(15)),
            keepalive_max: 3,
            ..Default::default()
        };

        let known_hosts_path = self.config.known_hosts_file();
        let handler = SshHandler {
            host: self.config.host.clone(),
            port: self.config.port,
            known_hosts_path: known_hosts_path.clone(),
        };

        let addr = format!("{}:{}", self.config.host, self.config.port);

        let mut handle = tokio::time::timeout(self.config.timeout, async {
            client::connect(Arc::new(ssh_config), addr, handler).await
        })
        .await
        .map_err(|_| SshError::Timeout("Connection timed out".to_string()))?
        .map_err(|e| {
            let msg = e.to_string();
            // Strip verbose OS error codes like "(os error 61)"
            let clean = if let Some(pos) = msg.find(" (os error") {
                &msg[..pos]
            } else {
                &msg
            };
            SshError::Connection(format!("Connection failed: {clean}"))
        })?;

        // Authenticate with SSH keys
        self.authenticate(&mut handle).await?;

        self.handle = Some(handle);

        Ok(())
    }

    /// Disconnect from the server
    pub async fn disconnect(&mut self) -> SshResult<()> {
        if let Some(handle) = self.handle.take() {
            handle
                .disconnect(Disconnect::ByApplication, "", "en")
                .await
                .map_err(|e| SshError::Connection(e.to_string()))?;
        }
        Ok(())
    }

    /// Check if connected
    pub fn is_connected(&self) -> bool {
        self.handle.is_some()
    }

    /// Get the config
    pub fn config(&self) -> &SshConfig {
        &self.config
    }

    pub fn authenticated_public_key(&self) -> Option<&str> {
        self.authenticated_public_key.as_deref()
    }
}

impl Drop for SshClient {
    fn drop(&mut self) {
        // Connection will be closed when handle is dropped
    }
}

/// Shell-safe single-quoting for interpolating values into remote commands.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests;
