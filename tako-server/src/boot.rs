use crate::SIGNAL_PARENT_ON_READY_ENV;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub(crate) async fn certificate_renewal_task(state: Arc<crate::ServerState>, interval: Duration) {
    tracing::info!(
        interval_hours = interval.as_secs() / 3600,
        "Starting certificate renewal task"
    );

    loop {
        tokio::time::sleep(interval).await;
        tracing::info!("Checking for certificates needing renewal…");

        let results = state.check_certificate_renewals().await;
        for result in results {
            match result {
                Ok(cert) => {
                    tracing::info!(
                        domain = %cert.domain,
                        expires_in_days = cert.days_until_expiry(),
                        "Certificate renewed successfully"
                    );
                }
                Err(e) => {
                    tracing::error!("Certificate renewal failed: {}", e);
                }
            }
        }
    }
}

pub(crate) fn install_rustls_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_some() {
        return;
    }

    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

pub(crate) fn should_signal_parent_on_ready() -> bool {
    matches!(
        std::env::var(SIGNAL_PARENT_ON_READY_ENV).as_deref(),
        Ok("1")
    )
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
pub(crate) struct ServerConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) server_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) acme_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) trusted_proxy: Option<ServerConfigTrustedProxy>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
pub(crate) struct ServerConfigTrustedProxy {
    #[serde(default)]
    pub(crate) proxy_protocol: bool,
    #[serde(default)]
    pub(crate) trusted_cidrs: Vec<String>,
    #[serde(default)]
    pub(crate) client_ip_headers: Vec<String>,
}

pub(crate) fn read_server_config(data_dir: &Path) -> ServerConfigFile {
    let config_path = data_dir.join("config.json");
    if let Ok(contents) = std::fs::read_to_string(&config_path)
        && let Ok(config) = serde_json::from_str::<ServerConfigFile>(&contents)
    {
        return config;
    }
    ServerConfigFile::default()
}

fn systemd_status(message: &str) {
    #[cfg(target_os = "linux")]
    if let Some(path) = std::env::var_os("NOTIFY_SOCKET") {
        use std::os::{
            linux::net::SocketAddrExt,
            unix::{
                ffi::OsStrExt,
                net::{SocketAddr, UnixDatagram},
            },
        };
        let address = match path.as_bytes().strip_prefix(b"@") {
            Some(name) => SocketAddr::from_abstract_name(name),
            None => SocketAddr::from_pathname(path),
        };
        if let (Ok(socket), Ok(address)) = (UnixDatagram::unbound(), address) {
            let _ = socket.send_to_addr(message.as_bytes(), &address);
        }
    }
}

pub(crate) fn starting(step: &str) {
    tracing::info!("Starting · {step}");
    systemd_status(&format!(
        "STATUS=Starting · {step}\nEXTEND_TIMEOUT_USEC=120000000"
    ));
}

pub(crate) fn sd_notify_ready() {
    #[cfg(unix)]
    {
        systemd_status(&format!(
            "STATUS=Ready\nREADY=1\nMAINPID={}",
            std::process::id()
        ));
        if !should_signal_parent_on_ready() {
            return;
        }
        let ppid = unsafe { libc::getppid() };
        if ppid > 1 {
            unsafe { libc::kill(ppid, libc::SIGUSR1) };
        }
    }
}

pub(crate) enum PrimaryStatus {
    Alive,
    IsUs,
    Down,
}

pub(crate) async fn probe_primary_socket(socket_path: &str, our_pid: u32) -> PrimaryStatus {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = match tokio::net::UnixStream::connect(socket_path).await {
        Ok(s) => s,
        Err(_) => return PrimaryStatus::Down,
    };

    let cmd = "{\"command\":\"server_info\"}\n";
    if stream.write_all(cmd.as_bytes()).await.is_err() {
        return PrimaryStatus::Down;
    }
    let _ = stream.shutdown().await;

    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf).await;
    let response = String::from_utf8_lossy(&buf);

    if !response.contains("\"status\":\"ok\"") {
        return PrimaryStatus::Down;
    }

    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&response)
        && let Some(pid) = parsed
            .get("data")
            .and_then(|d| d.get("pid"))
            .and_then(|p| p.as_u64())
        && pid as u32 == our_pid
    {
        return PrimaryStatus::IsUs;
    }

    PrimaryStatus::Alive
}
