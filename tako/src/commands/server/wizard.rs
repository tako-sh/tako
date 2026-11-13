use crate::output;
use std::path::Path;

mod connection;
mod ports;
pub(super) use connection::detect_server_target;
#[cfg(test)]
use connection::{parse_detected_arch, parse_detected_libc};
pub use ports::ServerPublicPorts;
pub(super) use ports::public_ports_from_cli;

pub async fn prompt_to_add_server(
    reason: &str,
    config_path: Option<&Path>,
) -> Result<Option<()>, Box<dyn std::error::Error>> {
    if !output::is_interactive() {
        return Ok(None);
    }
    output::warning(reason);
    if !output::confirm("Add a server now?", true)? {
        output::operation_cancelled();
        return Ok(None);
    }
    add(None, None, None, 22, None, None, None, config_path)
        .await
        .map(Some)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn add(
    host: Option<&str>,
    network: Option<&str>,
    name: Option<&str>,
    port: u16,
    key: Option<&Path>,
    ports: Option<ServerPublicPorts>,
    endpoint: Option<std::net::SocketAddr>,
    config_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let alias = crate::network::resolve_network(network, config_path)?;
    let host = match host {
        Some(host) => host.to_string(),
        None if output::is_interactive() => {
            let history = crate::config::CliHistoryToml::load().unwrap_or_default();
            output::TextField::new("Admin SSH target (admin@host)")
                .with_placeholder("ubuntu@203.0.113.10")
                .suggestions(&history.servers.hosts)
                .prompt()?
        }
        None => {
            return Err(
                "Server host is required. Run `tako network server add admin@host --network <network>`."
                    .into(),
            );
        }
    };
    let parsed = super::parse_add_host(&host);
    super::native::add(
        &parsed.host,
        parsed.admin_user.as_deref().unwrap_or("root"),
        &alias.name,
        name,
        port,
        key,
        ports.unwrap_or_default().into(),
        endpoint,
    )
    .await
}

#[cfg(test)]
mod tests;
