use crate::output;

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

fn parse_prompt_port(label: &str, value: &str) -> Result<u16, String> {
    let port = value
        .trim()
        .parse::<u16>()
        .map_err(|_| format!("{label} must be between 1 and 65535."))?;
    if port == 0 {
        return Err(format!("{label} must be between 1 and 65535."));
    }
    Ok(port)
}

fn prompt_public_ports(
    initial: Option<ServerPublicPorts>,
) -> Result<ServerPublicPorts, Box<dyn std::error::Error>> {
    let initial = initial.unwrap_or_default();
    loop {
        let http_default = initial.http_port.to_string();
        let https_default = initial.https_port.to_string();
        let http_port = output::TextField::new("HTTP port")
            .with_default(&http_default)
            .prompt_validated(|value| parse_prompt_port("HTTP port", value).map(|_| ()))?;
        let http_port = parse_prompt_port("HTTP port", &http_port)?;

        let https_port = output::TextField::new("HTTPS port")
            .with_default(&https_default)
            .prompt_validated(|value| parse_prompt_port("HTTPS port", value).map(|_| ()))?;
        let https_port = parse_prompt_port("HTTPS port", &https_port)?;

        let ports = ServerPublicPorts {
            http_port,
            https_port,
        };
        match validate_public_ports(ports) {
            Ok(()) => return Ok(ports),
            Err(message) if output::is_interactive() => output::warning(&message),
            Err(message) => return Err(message.into()),
        }
    }
}

pub(super) fn install_public_ports(
    requested: Option<ServerPublicPorts>,
) -> Result<ServerPublicPorts, Box<dyn std::error::Error>> {
    if let Some(ports) = requested {
        Ok(ports)
    } else if output::is_interactive() {
        prompt_public_ports(requested)
    } else {
        Ok(ServerPublicPorts::default())
    }
}
