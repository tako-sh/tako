#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerPublicPorts {
    pub http_port: u16,
    pub https_port: u16,
}

impl Default for ServerPublicPorts {
    fn default() -> Self {
        Self {
            http_port: 80,
            https_port: 443,
        }
    }
}

impl From<ServerPublicPorts> for crate::ssh::ServerInstallPorts {
    fn from(value: ServerPublicPorts) -> Self {
        Self {
            http_port: value.http_port,
            https_port: value.https_port,
        }
    }
}

pub(crate) fn public_ports_from_cli(
    http_port: Option<u16>,
    https_port: Option<u16>,
) -> Result<Option<ServerPublicPorts>, String> {
    if http_port.is_none() && https_port.is_none() {
        return Ok(None);
    }

    let ports = ServerPublicPorts {
        http_port: http_port.unwrap_or(80),
        https_port: https_port.unwrap_or(443),
    };
    validate_public_ports(ports)?;
    Ok(Some(ports))
}

fn validate_public_ports(ports: ServerPublicPorts) -> Result<(), String> {
    if ports.http_port == 0 {
        return Err("HTTP port must be between 1 and 65535.".to_string());
    }
    if ports.https_port == 0 {
        return Err("HTTPS port must be between 1 and 65535.".to_string());
    }
    if ports.http_port == ports.https_port {
        return Err("HTTP and HTTPS ports must differ.".to_string());
    }
    Ok(())
}

pub(super) fn install_public_ports(requested: Option<ServerPublicPorts>) -> ServerPublicPorts {
    requested.unwrap_or_default()
}
