use std::time::Duration;

use tako_core::{Command, Response, ServerRuntimeInfo};

use super::*;

impl SshClient {
    pub async fn tako_restart(&self) -> SshResult<()> {
        self.exec_checked(&Self::tako_restart_command()).await?;
        Ok(())
    }

    pub fn run_with_root_or_sudo(shell_script: &str) -> String {
        let escaped = shell_script.replace('\'', "'\\''");
        format!(
            "if [ \"$(id -u)\" -eq 0 ]; then sh -c '{0}'; elif command -v sudo >/dev/null 2>&1; then if sudo -n --preserve-env=GH_TOKEN,GITHUB_TOKEN true >/dev/null 2>&1; then sudo --preserve-env=GH_TOKEN,GITHUB_TOKEN sh -c '{0}'; else sudo sh -c '{0}'; fi; else echo \"error: this operation requires root privileges (run as root or install/configure sudo)\" >&2; exit 1; fi",
            escaped
        )
    }

    pub fn run_as_root(command: &str) -> String {
        format!(
            "if [ \"$(id -u)\" -eq 0 ]; then {0}; elif command -v sudo >/dev/null 2>&1; then sudo {0}; else echo \"error: this operation requires root privileges (run as root or install/configure sudo)\" >&2; exit 1; fi",
            command
        )
    }

    pub(super) fn tako_restart_command() -> String {
        Self::run_as_root(&format!("{TAKO_SERVER_SERVICE_HELPER} restart"))
    }

    pub(super) fn tako_reload_command() -> String {
        Self::run_as_root(&format!("{TAKO_SERVER_SERVICE_HELPER} reload"))
    }

    pub(super) fn tako_service_status_command() -> &'static str {
        "if command -v systemctl >/dev/null 2>&1; then systemctl is-active tako-server 2>/dev/null || echo unknown; elif command -v rc-service >/dev/null 2>&1; then if rc-service tako-server status >/dev/null 2>&1; then echo active; else echo inactive; fi; else echo unknown; fi"
    }

    fn service_start_hint() -> &'static str {
        "systemctl start tako-server (as root) or sudo systemctl start tako-server; rc-service tako-server start (as root) or sudo rc-service tako-server start"
    }

    pub fn tako_start_hint() -> &'static str {
        Self::service_start_hint()
    }

    pub async fn tako_reload(&self) -> SshResult<()> {
        self.exec_checked(&Self::tako_reload_command()).await?;
        Ok(())
    }

    pub async fn tako_status(&self) -> SshResult<String> {
        let list_probe = r#"{"command":"list"}"#;
        if self.tako_command_raw(list_probe).await.is_ok() {
            return Ok("active".to_string());
        }

        let output = self.exec(Self::tako_service_status_command()).await?;
        Ok(output.stdout.trim().to_string())
    }

    pub async fn tako_command(&self, json_command: &str) -> SshResult<String> {
        self.ensure_tako_hello().await?;
        self.tako_command_raw(json_command).await
    }

    async fn tako_command_raw(&self, json_command: &str) -> SshResult<String> {
        let mut payload = String::with_capacity(json_command.len() + 1);
        payload.push_str(json_command);
        payload.push('\n');
        let output = self
            .exec_checked_with_stdin(&Self::socket_request_command(), payload.as_bytes())
            .await?;
        Self::extract_socket_stdout(output)
    }

    pub(super) fn socket_request_command() -> String {
        Self::socket_request_command_on_path("/var/run/tako/tako.sock")
    }

    pub(super) fn socket_request_command_on_path(socket_path: &str) -> String {
        format!("nc -U '{}' | head -n 1", socket_path.replace('\'', "'\\''"))
    }

    pub(super) fn extract_socket_stdout(output: CommandOutput) -> SshResult<String> {
        if output.stdout.trim().is_empty() {
            let stderr = output.stderr.trim();
            if stderr.is_empty() {
                return Err(SshError::CommandFailed(
                    "tako-server socket returned an empty response".to_string(),
                ));
            }
            return Err(SshError::CommandFailed(stderr.to_string()));
        }
        Ok(output.stdout)
    }

    async fn ensure_tako_hello(&self) -> SshResult<()> {
        if self
            .tako_hello_checked
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(());
        }

        let cmd = Command::Hello {
            protocol_version: tako_core::PROTOCOL_VERSION,
        };
        let json =
            serde_json::to_string(&cmd).map_err(|e| SshError::CommandFailed(e.to_string()))?;
        let mut last_error: Option<SshError> = None;

        for attempt in 0..5 {
            let response_str = match self.tako_command_raw(&json).await {
                Ok(v) => v,
                Err(e) => {
                    last_error = Some(e);
                    if attempt < 4 {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                    return Err(last_error.unwrap_or_else(|| {
                        SshError::CommandFailed("tako-server handshake failed".to_string())
                    }));
                }
            };

            let response: Response = match serde_json::from_str(&response_str) {
                Ok(value) => value,
                Err(e) if e.is_eof() && attempt < 4 => {
                    last_error = Some(SshError::CommandFailed(e.to_string()));
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
                Err(e) => return Err(SshError::CommandFailed(e.to_string())),
            };

            Self::interpret_hello_response(&response).map_err(SshError::CommandFailed)?;
            self.tako_hello_checked
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return Ok(());
        }

        Err(last_error
            .unwrap_or_else(|| SshError::CommandFailed("tako-server handshake failed".to_string())))
    }

    pub async fn tako_app_status(&self, app_name: &str) -> SshResult<Response> {
        let cmd = Command::Status {
            app: app_name.to_string(),
        };
        let json =
            serde_json::to_string(&cmd).map_err(|e| SshError::CommandFailed(e.to_string()))?;
        let response_str = self.tako_command(&json).await?;
        let response: Response = serde_json::from_str(&response_str)
            .map_err(|e| SshError::CommandFailed(e.to_string()))?;
        Ok(response)
    }

    pub async fn tako_list_apps(&self) -> SshResult<Response> {
        let cmd = Command::List;
        let json =
            serde_json::to_string(&cmd).map_err(|e| SshError::CommandFailed(e.to_string()))?;
        let response_str = self.tako_command(&json).await?;
        let response: Response = serde_json::from_str(&response_str)
            .map_err(|e| SshError::CommandFailed(e.to_string()))?;
        Ok(response)
    }

    pub async fn tako_routes(&self) -> SshResult<Response> {
        let cmd = Command::Routes;
        let json =
            serde_json::to_string(&cmd).map_err(|e| SshError::CommandFailed(e.to_string()))?;
        let response_str = self.tako_command(&json).await?;
        let response: Response = serde_json::from_str(&response_str)
            .map_err(|e| SshError::CommandFailed(e.to_string()))?;
        Ok(response)
    }

    pub async fn tako_server_info(&self) -> SshResult<ServerRuntimeInfo> {
        let cmd = Command::ServerInfo;
        let json =
            serde_json::to_string(&cmd).map_err(|e| SshError::CommandFailed(e.to_string()))?;
        let response_str = self.tako_command(&json).await?;
        parse_ok_data_response(response_str)
    }

    pub async fn tako_enter_upgrading(&self, owner: &str) -> SshResult<()> {
        let cmd = Command::EnterUpgrading {
            owner: owner.to_string(),
        };
        let json =
            serde_json::to_string(&cmd).map_err(|e| SshError::CommandFailed(e.to_string()))?;
        let response_str = self.tako_command(&json).await?;
        parse_ok_unit_response(response_str)
    }

    pub async fn tako_exit_upgrading(&self, owner: &str) -> SshResult<()> {
        let cmd = Command::ExitUpgrading {
            owner: owner.to_string(),
        };
        let json =
            serde_json::to_string(&cmd).map_err(|e| SshError::CommandFailed(e.to_string()))?;
        let response_str = self.tako_command(&json).await?;
        parse_ok_unit_response(response_str)
    }

    pub async fn tako_command_on_socket(
        &self,
        socket_path: &str,
        json_command: &str,
    ) -> SshResult<String> {
        let mut payload = String::with_capacity(json_command.len() + 1);
        payload.push_str(json_command);
        payload.push('\n');
        let output = self
            .exec_checked_with_stdin(
                &Self::socket_request_command_on_path(socket_path),
                payload.as_bytes(),
            )
            .await?;
        Self::extract_socket_stdout(output)
    }

    pub async fn tako_hello_on_socket(&self, socket_path: &str) -> SshResult<()> {
        let cmd = Command::Hello {
            protocol_version: tako_core::PROTOCOL_VERSION,
        };
        let json =
            serde_json::to_string(&cmd).map_err(|e| SshError::CommandFailed(e.to_string()))?;
        let response_str = self.tako_command_on_socket(socket_path, &json).await?;
        let response: Response = serde_json::from_str(&response_str)
            .map_err(|e| SshError::CommandFailed(e.to_string()))?;
        Self::interpret_hello_response(&response).map_err(SshError::CommandFailed)
    }

    pub fn clear_tako_hello_cache(&mut self) {
        self.tako_hello_checked
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

fn parse_ok_unit_response(response_str: String) -> SshResult<()> {
    let response: Response =
        serde_json::from_str(&response_str).map_err(|e| SshError::CommandFailed(e.to_string()))?;
    match response {
        Response::Ok { .. } => Ok(()),
        Response::Error { message } => Err(SshError::CommandFailed(message)),
    }
}

fn parse_ok_data_response<T: serde::de::DeserializeOwned>(response_str: String) -> SshResult<T> {
    let response: Response =
        serde_json::from_str(&response_str).map_err(|e| SshError::CommandFailed(e.to_string()))?;
    match response {
        Response::Ok { data } => {
            serde_json::from_value(data).map_err(|e| SshError::CommandFailed(e.to_string()))
        }
        Response::Error { message } => Err(SshError::CommandFailed(message)),
    }
}
