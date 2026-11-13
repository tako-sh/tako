use std::net::Ipv6Addr;

use tako_network::{BootstrapRequest, BootstrapResponse};

use super::super::error::{SshError, SshResult};
use super::{ServerInstallPorts, SshClient};

impl SshClient {
    pub async fn install_native_tako_server(&self, ports: ServerInstallPorts) -> SshResult<()> {
        let key = self
            .authenticated_public_key()
            .ok_or_else(|| SshError::Authentication("No authenticated operator SSH key".into()))?;
        let env = super::tako::install_server_env(key, Some(ports));
        let command = Self::run_with_root_or_sudo(&format!("{env} TAKO_VPN_PROVIDER=native sh -s"));
        self.exec_checked_with_stdin(&command, super::tako::INSTALL_SERVER_SCRIPT.as_bytes())
            .await
            .map_err(|error| native_command_error("Install native network components", error))?;
        Ok(())
    }

    pub async fn native_bootstrap(
        &self,
        request: &BootstrapRequest,
    ) -> SshResult<BootstrapResponse> {
        let bytes = serde_json::to_vec(request)
            .map_err(|error| SshError::CommandFailed(error.to_string()))?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(SshError::CommandFailed(
                "Native bootstrap request exceeds 4 MiB".into(),
            ));
        }
        let command = Self::run_with_root_or_sudo(
            "runuser -u tako -- /usr/local/bin/tako-server --data-dir /opt/tako --network-bootstrap",
        );
        let output = self
            .exec_checked_with_stdin(&command, &bytes)
            .await
            .map_err(|error| native_command_error("Bootstrap native server identity", error))?;
        if output.stdout.len() > 4 * 1024 * 1024 {
            return Err(SshError::CommandFailed(
                "Native bootstrap response exceeds 4 MiB".into(),
            ));
        }
        serde_json::from_str(&output.stdout)
            .map_err(|error| SshError::CommandFailed(error.to_string()))
    }

    pub async fn start_native_tako_server(
        &self,
        ports: ServerInstallPorts,
        address: Ipv6Addr,
    ) -> SshResult<()> {
        let command = Self::run_with_root_or_sudo(&format!(
            "set -eu; systemctl enable --now tako-network-agent.service; \
             count=0; while ! /usr/local/libexec/tako/native-address '{address}' >/dev/null 2>&1; do \
             count=$((count + 1)); if [ \"$count\" -ge 30 ]; then echo 'error: native network did not become ready' >&2; exit 1; fi; sleep 1; done; \
             /usr/local/bin/tako-server-service configure {} {} native '{address}'",
            ports.http_port, ports.https_port,
        ));
        self.exec_checked(&command)
            .await
            .map_err(|error| native_command_error("Start native network services", error))?;
        Ok(())
    }
}

fn native_command_error(action: &str, error: SshError) -> SshError {
    match error {
        SshError::NonZeroExit { code, stderr } => {
            let detail = stderr.trim();
            let suffix = if detail.is_empty() {
                String::new()
            } else {
                // Bootstrap stdout can contain private identity material. Only
                // include the remote diagnostic stream, with a bounded tail.
                let start = detail
                    .char_indices()
                    .rev()
                    .take(4096)
                    .last()
                    .map_or(0, |(index, _)| index);
                format!("\n{}", &detail[start..])
            };
            SshError::CommandFailed(format!("{action} failed (exit {code}){suffix}"))
        }
        error => SshError::CommandFailed(format!("{action}: {error}")),
    }
}
