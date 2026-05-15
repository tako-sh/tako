use crate::output;
use crate::ssh::{SshClient, SshConfig};
use std::error::Error;

pub(super) async fn configure_server(name: Option<&str>) -> Result<(), Box<dyn Error>> {
    use crate::config::ServersToml;

    let servers = ServersToml::load()?;
    let name = match resolve_configure_target(name, &servers, output::is_interactive())? {
        ConfigureTarget::Name(name) => name,
        ConfigureTarget::Select(options) => {
            match output::select("Select server to configure", None, options) {
                Ok(name) => name,
                Err(e) if output::is_wizard_back(&e) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
    };

    if !output::is_interactive() {
        return Err("Interactive server configuration requires a terminal.".into());
    }

    let server = servers
        .get(&name)
        .ok_or_else(|| format!("Server '{}' not found.", name))?;
    let (mut ssh, current_config) = connect_and_read_server_config(&name, server).await?;
    let result = configure_server_settings(&name, &ssh, &current_config).await;
    let _ = ssh.disconnect().await;
    result
}

async fn configure_server_settings(
    name: &str,
    ssh: &SshClient,
    current_config: &super::remote_config::ServerConfigWithoutSecrets,
) -> Result<(), Box<dyn Error>> {
    for step in configure_server_steps() {
        match step {
            ConfigureServerStep::SourceIp => {
                super::trusted_proxy::configure_trusted_proxy(name, ssh, current_config).await?
            }
            ConfigureServerStep::DnsWildcardCertificates => {
                super::dns::configure_dns(name, ssh, current_config).await?
            }
        }
    }

    Ok(())
}

async fn connect_and_read_server_config(
    name: &str,
    server: &crate::config::ServerEntry,
) -> Result<(SshClient, super::remote_config::ServerConfigWithoutSecrets), Box<dyn Error>> {
    let ssh_config = SshConfig::from_server(&server.host, server.port);
    let mut ssh = SshClient::new(ssh_config);

    let _t = output::timed(&read_server_config_trace_label(name));
    ssh.connect()
        .await
        .map_err(|e| -> Box<dyn Error> { format!("Failed to connect to {name}: {e}").into() })?;
    let current_config = super::remote_config::read_server_config_without_secrets(&ssh)
        .await
        .map_err(|e| -> Box<dyn Error> {
            format!("Failed to read server config from {name}: {e}").into()
        })?;

    Ok((ssh, current_config))
}

fn read_server_config_trace_label(name: &str) -> String {
    format!("[{name}] Read server config")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigureServerStep {
    SourceIp,
    DnsWildcardCertificates,
}

const CONFIGURE_SERVER_STEPS: &[ConfigureServerStep] = &[
    ConfigureServerStep::SourceIp,
    ConfigureServerStep::DnsWildcardCertificates,
];

fn configure_server_steps() -> &'static [ConfigureServerStep] {
    CONFIGURE_SERVER_STEPS
}

#[derive(Debug, PartialEq, Eq)]
enum ConfigureTarget {
    Name(String),
    Select(Vec<(String, String)>),
}

fn resolve_configure_target(
    name: Option<&str>,
    servers: &crate::config::ServersToml,
    interactive: bool,
) -> Result<ConfigureTarget, String> {
    if servers.is_empty() {
        return Err("No servers configured. Run `tako servers add` first.".to_string());
    }

    if let Some(name) = name {
        if !servers.contains(name) {
            return Err(format!("Server '{}' not found.", name));
        }
        return Ok(ConfigureTarget::Name(name.to_string()));
    }

    if servers.len() == 1 {
        let name =
            servers.names().into_iter().next().ok_or_else(|| {
                "No servers configured. Run `tako servers add` first.".to_string()
            })?;
        return Ok(ConfigureTarget::Name(name.to_string()));
    }

    if !interactive {
        return Err(
            "No server name provided and selection requires an interactive terminal. Run 'tako servers configure <name>'."
                .to_string(),
        );
    }

    Ok(ConfigureTarget::Select(configure_server_options(servers)))
}

fn configure_server_options(servers: &crate::config::ServersToml) -> Vec<(String, String)> {
    let mut names = servers.names();
    names.sort_unstable();
    names
        .into_iter()
        .filter_map(|name| {
            servers
                .get(name)
                .map(|entry| (configure_server_option_label(name, entry), name.to_string()))
        })
        .collect()
}

fn configure_server_option_label(name: &str, entry: &crate::config::ServerEntry) -> String {
    match entry.description.as_deref().map(str::trim) {
        Some(description) if !description.is_empty() => {
            format!("{name} ({description})  {}:{}", entry.host, entry.port)
        }
        _ => format!("{name}  {}:{}", entry.host, entry.port),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ServerEntry, ServersToml};

    fn server(host: &str, description: Option<&str>) -> ServerEntry {
        ServerEntry {
            host: host.to_string(),
            description: description.map(str::to_string),
            ..ServerEntry::default()
        }
    }

    #[test]
    fn resolve_configure_target_uses_requested_server() {
        let mut servers = ServersToml::default();
        servers
            .add("prod".to_string(), server("prod.example.com", None))
            .unwrap();

        let target = resolve_configure_target(Some("prod"), &servers, false).unwrap();

        match target {
            ConfigureTarget::Name(name) => assert_eq!(name, "prod"),
            ConfigureTarget::Select(_) => panic!("expected resolved name"),
        }
    }

    #[test]
    fn resolve_configure_target_rejects_unknown_requested_server() {
        let mut servers = ServersToml::default();
        servers
            .add("prod".to_string(), server("prod.example.com", None))
            .unwrap();

        let err = resolve_configure_target(Some("staging"), &servers, false).unwrap_err();

        assert_eq!(err, "Server 'staging' not found.");
    }

    #[test]
    fn resolve_configure_target_auto_selects_single_server() {
        let mut servers = ServersToml::default();
        servers
            .add("prod".to_string(), server("prod.example.com", None))
            .unwrap();

        let target = resolve_configure_target(None, &servers, false).unwrap();

        match target {
            ConfigureTarget::Name(name) => assert_eq!(name, "prod"),
            ConfigureTarget::Select(_) => panic!("expected resolved name"),
        }
    }

    #[test]
    fn resolve_configure_target_prompts_for_multiple_servers_when_interactive() {
        let mut servers = ServersToml::default();
        servers
            .add(
                "prod".to_string(),
                server("prod.example.com", Some("Primary")),
            )
            .unwrap();
        servers
            .add("staging".to_string(), server("staging.example.com", None))
            .unwrap();

        let target = resolve_configure_target(None, &servers, true).unwrap();

        match target {
            ConfigureTarget::Name(_) => panic!("expected selection"),
            ConfigureTarget::Select(options) => assert_eq!(
                options,
                vec![
                    (
                        "prod (Primary)  prod.example.com:22".to_string(),
                        "prod".to_string(),
                    ),
                    (
                        "staging  staging.example.com:22".to_string(),
                        "staging".to_string(),
                    ),
                ],
            ),
        }
    }

    #[test]
    fn resolve_configure_target_requires_name_for_multiple_servers_without_terminal() {
        let mut servers = ServersToml::default();
        servers
            .add("prod".to_string(), server("prod.example.com", None))
            .unwrap();
        servers
            .add("staging".to_string(), server("staging.example.com", None))
            .unwrap();

        let err = resolve_configure_target(None, &servers, false).unwrap_err();

        assert_eq!(
            err,
            "No server name provided and selection requires an interactive terminal. Run 'tako servers configure <name>'.",
        );
    }

    #[test]
    fn configure_server_steps_start_with_source_ip_then_dns_wildcards() {
        assert_eq!(
            configure_server_steps(),
            &[
                ConfigureServerStep::SourceIp,
                ConfigureServerStep::DnsWildcardCertificates,
            ],
        );
    }

    #[test]
    fn read_server_config_trace_label_includes_server_name() {
        assert_eq!(
            read_server_config_trace_label("prod"),
            "[prod] Read server config",
        );
    }
}
