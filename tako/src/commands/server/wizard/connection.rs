pub(super) struct WizardConnectionResult {
    pub(super) target: crate::config::ServerTarget,
    pub(super) version: Option<String>,
    pub(super) installed: bool,
    pub(super) server_name: Option<String>,
}

pub(super) async fn check_tako_connection(
    host: &str,
    port: u16,
) -> Result<WizardConnectionResult, String> {
    use crate::ssh::{SshClient, SshConfig};

    let ssh_config = SshConfig::from_server(host, port);
    let mut ssh = SshClient::new(ssh_config);
    ssh.connect().await.map_err(|e| e.to_string())?;

    let result = async {
        let target = detect_server_target(&ssh)
            .await
            .map_err(|e| format!("Target detection failed: {e}"))?;
        tracing::debug!("Detected target: {}", target.label());

        let (installed, version, server_name) = match ssh.is_tako_installed().await {
            Ok(true) => {
                let ver = ssh.tako_version().await.ok().flatten();
                let sn = ssh
                    .tako_server_info()
                    .await
                    .ok()
                    .and_then(|info| info.server_name);
                (true, ver, sn)
            }
            Ok(false) => (false, None, None),
            Err(_) => (false, None, None),
        };

        if installed {
            ssh.enroll_management_key()
                .await
                .map_err(|e| format!("Management key enrollment failed: {e}"))?;
        }

        Ok(WizardConnectionResult {
            target,
            version,
            installed,
            server_name,
        })
    }
    .await;

    let disconnect = ssh.disconnect().await.map_err(|e| e.to_string());
    match (result, disconnect) {
        (Ok(info), Ok(())) => Ok(info),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), _) => Err(error),
    }
}

pub(super) async fn install_tako_server_with_admin(
    host: &str,
    port: u16,
    admin_user: &str,
) -> Result<(), String> {
    use crate::ssh::{SshClient, SshConfig};

    let ssh_config = SshConfig::for_user(host, port, admin_user);
    let mut ssh = SshClient::new(ssh_config);
    ssh.connect().await.map_err(|e| e.to_string())?;

    let result = ssh
        .install_tako_server()
        .await
        .map_err(|e| format!("Install failed: {e}"));
    let disconnect = ssh.disconnect().await.map_err(|e| e.to_string());
    match (result, disconnect) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), _) => Err(error),
    }
}

pub(super) async fn verify_remote_management(
    host: &str,
) -> Result<crate::management_http::ManagementProbe, String> {
    let probe = crate::management_http::probe(host).await.map_err(|error| {
        tracing::debug!("Remote management probe failed: {error}");
        remote_management_unavailable_message()
    })?;

    let mut client = crate::management_http::ManagementClient::new(host)
        .await
        .map_err(|error| {
            tracing::debug!("Remote management auth setup failed: {error}");
            remote_management_unavailable_message()
        })?;
    client
        .send(&tako_core::Command::List)
        .await
        .map_err(|error| {
            tracing::debug!("Remote management signed probe failed: {error}");
            remote_management_unavailable_message()
        })?;

    Ok(probe)
}

pub(super) async fn verify_tailscale_host(host: &str) -> Result<(), String> {
    crate::tailscale::ensure_tailscale_host(host)
        .await
        .map_err(|_| remote_management_unavailable_message())
}

pub(super) fn remote_management_unavailable_message() -> String {
    format!(
        "{} Connect this machine and the server to Tailscale, then run `tako servers add` with the server's MagicDNS name.",
        crate::tailscale::required_message()
    )
}

pub(super) fn trace_management_probe(host: &str, probe: &crate::management_http::ManagementProbe) {
    let identity = probe
        .info
        .server_identity
        .as_ref()
        .or(probe.hello.server_identity.as_ref())
        .map(String::as_str)
        .unwrap_or("unknown");
    tracing::debug!(host, server_identity = identity, "Remote management ready");
}

const DETECT_LIBC_COMMAND: &str = "if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then echo musl; \
elif command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -Eqi 'glibc|gnu libc|gnu c library'; then echo glibc; \
elif command -v getconf >/dev/null 2>&1 && getconf GNU_LIBC_VERSION >/dev/null 2>&1; then echo glibc; \
elif ls /lib/ld-musl-*.so.1 /usr/lib/ld-musl-*.so.1 >/dev/null 2>&1; then echo musl; \
else echo unknown; fi";

pub(in crate::commands::server) async fn detect_server_target(
    ssh: &crate::ssh::SshClient,
) -> Result<crate::config::ServerTarget, String> {
    let combined = format!(
        "echo ARCH:$(uname -m 2>/dev/null || echo unknown); echo LIBC:$({})",
        DETECT_LIBC_COMMAND
    );
    let output = ssh
        .exec(&combined)
        .await
        .map_err(|e| format!("Failed to detect server target: {}", e))?;

    let mut arch_str = String::new();
    let mut libc_str = String::new();
    for line in output.stdout.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("ARCH:") {
            arch_str = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("LIBC:") {
            libc_str = val.trim().to_string();
        }
    }

    let arch = parse_detected_arch(&arch_str)?;
    let libc = parse_detected_libc(&libc_str)?;

    crate::config::ServerTarget::normalized(&arch, &libc)
        .map_err(|e| format!("Unsupported target metadata: {}", e))
}

pub(super) fn parse_detected_arch(stdout: &str) -> Result<String, String> {
    let raw = stdout.lines().map(str::trim).find(|line| !line.is_empty());
    let Some(raw_arch) = raw else {
        return Err("Could not detect server architecture from `uname -m` output".to_string());
    };

    crate::config::ServerTarget::normalize_arch(raw_arch).ok_or_else(|| {
        format!(
            "Unsupported server architecture '{}'. Supported architectures: x86_64, aarch64.",
            raw_arch
        )
    })
}

pub(super) fn parse_detected_libc(stdout: &str) -> Result<String, String> {
    let raw = stdout.lines().map(str::trim).find(|line| !line.is_empty());
    let Some(raw_libc) = raw else {
        return Err("Could not detect server libc".to_string());
    };

    crate::config::ServerTarget::normalize_libc(raw_libc).ok_or_else(|| {
        format!(
            "Unsupported server libc '{}'. Supported libc families: glibc, musl.",
            raw_libc
        )
    })
}
