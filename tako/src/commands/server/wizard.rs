use crate::output;
use tracing::Instrument;

mod connection;
mod naming;

pub(super) use connection::detect_server_target;
use connection::{
    WizardConnectionResult, check_tako_connection, configure_tako_server_with_service_user,
    install_tako_server_with_admin, trace_management_probe, verify_remote_management,
    verify_tailscale_host,
};
#[cfg(test)]
use connection::{parse_detected_arch, parse_detected_libc, remote_management_unavailable_message};
use naming::{
    append_unique_suggestions, default_server_name_from_host, next_available_server_name,
    push_unique_suggestion, record_server_history,
};

pub async fn prompt_to_add_server(
    reason: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if !output::is_interactive() {
        return Ok(None);
    }

    output::warning(reason);

    let should_add = output::confirm("Add a server now?", true)?;
    if !should_add {
        output::operation_cancelled();
        return Ok(None);
    }

    run_add_server_wizard(None, None, 22, None, true, true, None).await
}

pub struct AddServerOptions<'a> {
    pub name: Option<&'a str>,
    pub description: Option<&'a str>,
    pub port: u16,
    pub public_ports: Option<ServerPublicPorts>,
    pub no_test: bool,
    pub pre_detected_target: Option<crate::config::ServerTarget>,
    pub pre_detected_public_ports: Option<ServerPublicPorts>,
    pub install_if_missing: bool,
    pub allow_install_prompt: bool,
    pub admin_user: Option<&'a str>,
}

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

pub(super) fn public_ports_from_cli(
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

fn install_public_ports(
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

async fn apply_first_run_settings_and_start(
    host: &str,
    port: u16,
    public_ports: ServerPublicPorts,
    settings: &super::first_run::FirstRunServerSettings,
) -> Result<(), Box<dyn std::error::Error>> {
    super::first_run::apply_first_run_settings_before_start(host, port, host, settings).await?;

    let start_scope = output::scope(host);
    let _t = output::timed(&format!("Start tako-server on {host}:{port}"));
    output::with_spinner_async_err(
        "Starting tako-server",
        "tako-server started",
        "Start failed",
        configure_tako_server_with_service_user(host, port, Some(public_ports))
            .instrument(start_scope),
    )
    .await?;
    drop(_t);

    Ok(())
}

pub(super) async fn run_add_server_wizard(
    initial_name: Option<&str>,
    initial_description: Option<&str>,
    initial_port: u16,
    initial_public_ports: Option<ServerPublicPorts>,
    default_test_ssh: bool,
    allow_install: bool,
    admin_user_default: Option<&str>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    use crate::config::{CliHistoryToml, ServersToml};

    if !output::is_interactive() {
        return Err(
            "Interactive server setup requires a terminal. Run: tako servers add <host>".into(),
        );
    }

    let existing_servers = ServersToml::load()?;
    let suggestion_history = CliHistoryToml::load().unwrap_or_default();

    let host_suggestions = suggestion_history.server_host_suggestions();
    let mut name_suggestions = suggestion_history.server_name_suggestions();
    let mut port_suggestions = suggestion_history.server_port_suggestions();
    let mut host_suggestions = host_suggestions;

    // Collect existing hosts/names for filtering placeholders
    let existing_hosts: Vec<String> = existing_servers
        .names()
        .iter()
        .filter_map(|n| existing_servers.get(n).map(|s| s.host.clone()))
        .collect();

    for server_name in existing_servers.names() {
        if let Some(server) = existing_servers.get(server_name) {
            push_unique_suggestion(&mut host_suggestions, server.host.clone());
            push_unique_suggestion(&mut name_suggestions, server_name.to_string());
            push_unique_suggestion(&mut port_suggestions, server.port.to_string());
        }
    }

    push_unique_suggestion(&mut port_suggestions, String::from("22"));
    push_unique_suggestion(&mut port_suggestions, initial_port.to_string());

    if let Some(initial_name) = initial_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        push_unique_suggestion(&mut name_suggestions, initial_name.to_string());
    }

    // Placeholder: most recent history entry not already in servers
    let host_placeholder = host_suggestions
        .iter()
        .find(|h| !existing_hosts.contains(h))
        .cloned();

    // --- Wizard 1: Connection details ---
    let mut conn_wizard =
        output::Wizard::new().with_fields(&[("Server IP or hostname", false), ("SSH port", false)]);
    let mut step = 0usize;
    let mut host = String::new();
    let mut port: u16 = initial_port;

    loop {
        match step {
            0 => {
                let mut builder =
                    output::TextField::new("Server IP or hostname").suggestions(&host_suggestions);
                if !host.is_empty() {
                    builder = builder.with_default(&host);
                } else if let Some(ref ph) = host_placeholder {
                    builder = builder.with_placeholder(ph);
                }
                match conn_wizard.text_field(builder) {
                    Ok(v) => {
                        let v = v.trim().to_string();
                        if v.is_empty() {
                            return Err("Server host cannot be empty".into());
                        }
                        host = v;
                        step = 1;
                    }
                    Err(e) if output::is_wizard_back(&e) => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
            }
            1 => {
                let port_str = port.to_string();
                let mut port_prompt_suggestions =
                    suggestion_history.server_port_suggestions_for(&host, "");
                for server_name in existing_servers.names() {
                    if let Some(server) = existing_servers.get(server_name)
                        && server.host == host
                    {
                        push_unique_suggestion(
                            &mut port_prompt_suggestions,
                            server.port.to_string(),
                        );
                    }
                }
                append_unique_suggestions(&mut port_prompt_suggestions, &port_suggestions);
                match conn_wizard.text_field(
                    output::TextField::new("SSH port")
                        .with_default(&port_str)
                        .suggestions(&port_prompt_suggestions),
                ) {
                    Ok(v) => match v.trim().parse::<u16>() {
                        Ok(p) => {
                            port = p;
                            break;
                        }
                        Err(_) => {
                            output::warning(&format!("Invalid SSH port '{}'", v.trim()));
                            conn_wizard.undo_last();
                        }
                    },
                    Err(e) if output::is_wizard_back(&e) => step = 0,
                    Err(e) => return Err(e.into()),
                }
            }
            _ => break,
        }
    }

    if crate::ssh::configured_key_passphrase().is_none()
        && crate::ssh::default_key_needs_passphrase()
    {
        let passphrase = output::TextField::new("SSH passphrase")
            .password()
            .optional()
            .prompt()?;
        crate::ssh::set_key_passphrase(Some(passphrase));
    }

    // --- SSH connection test ---
    let mut remote_server_name: Option<String> = None;
    let mut detected_target: Option<crate::config::ServerTarget> = None;
    let mut detected_public_ports: Option<ServerPublicPorts> = initial_public_ports;

    if default_test_ssh {
        let host_span = output::scope(&host);
        let _t = output::timed(&format!("Test SSH connection to {host}:{port}"));
        let mut result: Result<WizardConnectionResult, String> = output::with_spinner_async_err(
            "Connecting",
            "Connection successful",
            "Connection failed",
            check_tako_connection(&host, port).instrument(host_span),
        )
        .await;
        drop(_t);

        let needs_install = match &result {
            Ok(info) => !info.installed,
            Err(_) => true,
        };
        if allow_install && needs_install {
            let should_install = output::confirm("Install tako-server now?", true)?;
            if should_install {
                let public_ports = install_public_ports(initial_public_ports)?;
                let admin_user = output::TextField::new("Admin SSH user")
                    .with_default(admin_user_default.unwrap_or("root"))
                    .prompt()?;
                let first_run_settings = super::first_run::prompt_first_run_settings()?;
                let install_scope = output::scope(&host);
                let _t = output::timed(&format!("Install tako-server on {host}:{port}"));
                output::with_spinner_async_err(
                    "Installing tako-server",
                    "tako-server installed",
                    "Install failed",
                    install_tako_server_with_admin(
                        &host,
                        port,
                        &admin_user,
                        Some(public_ports),
                        crate::ssh::InstallServerMode::BootstrapOnly,
                    )
                    .instrument(install_scope),
                )
                .await?;
                drop(_t);
                detected_public_ports = Some(public_ports);
                apply_first_run_settings_and_start(&host, port, public_ports, &first_run_settings)
                    .await?;

                let verify_scope = output::scope(&host);
                let _t = output::timed(&format!("Verify tako-server on {host}:{port}"));
                result = output::with_spinner_async_err(
                    "Verifying install",
                    "Install verified",
                    "Verification failed",
                    check_tako_connection(&host, port).instrument(verify_scope),
                )
                .await;
                drop(_t);
            }
        }

        if allow_install
            && matches!(
                &result,
                Ok(info) if info.installed && info.public_ports.is_none()
            )
        {
            let should_configure = output::confirm("Set up and start tako-server now?", true)?;
            if should_configure {
                let public_ports = install_public_ports(initial_public_ports)?;
                let first_run_settings = super::first_run::prompt_first_run_settings()?;
                apply_first_run_settings_and_start(&host, port, public_ports, &first_run_settings)
                    .await?;
                detected_public_ports = Some(public_ports);

                let verify_scope = output::scope(&host);
                let _t = output::timed(&format!("Verify tako-server on {host}:{port}"));
                result = output::with_spinner_async_err(
                    "Verifying server",
                    "Server verified",
                    "Verification failed",
                    check_tako_connection(&host, port).instrument(verify_scope),
                )
                .await;
                drop(_t);
            }
        }

        let _host_scope = output::scope(&host).entered();
        match result {
            Ok(info) => {
                tracing::debug!("Target: {}", info.target.label());
                if let Some(ref ver) = info.version {
                    let ver = ver.strip_prefix("tako-server ").unwrap_or(ver);
                    tracing::debug!("Server version: {ver}");
                }
                if !info.installed {
                    return Err(server_not_installed_message().into());
                }
                remote_server_name = info.server_name;
                detected_target = Some(info.target);
                detected_public_ports = info.public_ports.or(detected_public_ports);
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    }

    if output::is_pretty() {
        eprintln!();
    }

    // --- Wizard 2: Server identity ---
    let mut id_wizard =
        output::Wizard::new().with_fields(&[("Server name", false), ("Description", false)]);
    let mut step = 0usize;
    let mut name = String::new();
    let mut description = String::new();

    loop {
        match step {
            0 => {
                let mut name_prompt_suggestions =
                    suggestion_history.server_name_suggestions_for_host(&host);
                for server_name in existing_servers.names() {
                    if let Some(server) = existing_servers.get(server_name)
                        && server.host == host
                    {
                        push_unique_suggestion(
                            &mut name_prompt_suggestions,
                            server_name.to_string(),
                        );
                    }
                }
                append_unique_suggestions(&mut name_prompt_suggestions, &name_suggestions);
                push_unique_suggestion(&mut name_prompt_suggestions, host.clone());

                let default_name = if !name.is_empty() {
                    Some(name.as_str())
                } else if let Some(n) = initial_name {
                    Some(n)
                } else if let Some(ref rsn) = remote_server_name {
                    Some(rsn.as_str())
                } else if let Some(n) = name_prompt_suggestions.first() {
                    Some(n.as_str())
                } else if !host.chars().next().is_some_and(|c| c.is_ascii_digit())
                    && !host.contains(':')
                {
                    Some(host.as_str())
                } else {
                    None
                };
                match id_wizard.text_field(
                    output::TextField::new("Server name")
                        .default_opt(default_name)
                        .suggestions(&name_prompt_suggestions),
                ) {
                    Ok(v) => {
                        name = v.trim().to_string();
                        step = 1;
                    }
                    Err(e) if output::is_wizard_back(&e) => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
            }
            1 => {
                let default_desc = if !description.is_empty() {
                    Some(description.as_str())
                } else {
                    initial_description
                };
                match id_wizard.text_field(
                    output::TextField::new("Description")
                        .optional()
                        .default_opt(default_desc),
                ) {
                    Ok(v) => {
                        description = v.trim().to_string();
                        break;
                    }
                    Err(e) if output::is_wizard_back(&e) => step = 0,
                    Err(e) => return Err(e.into()),
                }
            }
            _ => break,
        }
    }

    let name_ref = Some(name.as_str());
    let description_ref = if description.is_empty() {
        None
    } else {
        Some(description.as_str())
    };

    // SSH was already tested above; skip re-testing in add_server
    add_server(
        &host,
        AddServerOptions {
            name: name_ref,
            description: description_ref,
            port,
            public_ports: initial_public_ports,
            no_test: true,
            pre_detected_target: detected_target,
            pre_detected_public_ports: detected_public_ports,
            install_if_missing: false,
            allow_install_prompt: false,
            admin_user: None,
        },
    )
    .await
}

pub async fn add_server(
    host: &str,
    options: AddServerOptions<'_>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    use crate::config::{ServerEntry, ServerTarget, ServersToml};

    let AddServerOptions {
        name,
        description,
        port,
        public_ports,
        no_test,
        pre_detected_target,
        pre_detected_public_ports,
        install_if_missing,
        allow_install_prompt,
        admin_user,
    } = options;

    let mut servers = ServersToml::load()?;
    let normalized_description = description.and_then(|d| {
        let trimmed = d.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });

    let explicit_name = name.map(str::trim).filter(|value| !value.is_empty());
    let mut server_name = explicit_name
        .map(ToOwned::to_owned)
        .or_else(|| default_server_name_from_host(host))
        .ok_or(
            "Server name is required for this host. Use --name <name>, or run 'tako servers add' to use the interactive wizard.",
        )?;

    if servers.contains(&server_name) && explicit_name.is_none() && output::is_interactive() {
        let suggested = next_available_server_name(&server_name, &servers);
        server_name = output::TextField::new("Server name")
            .with_default(&suggested)
            .prompt()?
            .trim()
            .to_string();
    }

    let server_name = server_name.trim().to_string();
    if server_name.is_empty() {
        return Err("Server name is required.".into());
    }

    let mut resolved_public_ports = pre_detected_public_ports.or(public_ports);

    // Check if host already exists
    if let Some(existing_name) = servers.find_by_host(host) {
        let existing_name = existing_name.to_string();
        let existing = servers
            .get(&existing_name)
            .cloned()
            .ok_or_else(|| format!("Server '{}' vanished during lookup", existing_name))?;

        if existing_name == server_name && existing.port == port {
            let desired_public_ports = resolved_public_ports.unwrap_or(ServerPublicPorts {
                http_port: existing.http_port,
                https_port: existing.https_port,
            });
            let description_changed = normalized_description.is_some()
                && existing.description.as_deref() != normalized_description.as_deref();
            let public_ports_changed = desired_public_ports.http_port != existing.http_port
                || desired_public_ports.https_port != existing.https_port;

            if description_changed || public_ports_changed {
                let desired_description = normalized_description
                    .clone()
                    .or_else(|| existing.description.clone());
                servers.update(
                    &existing_name,
                    ServerEntry {
                        host: existing.host,
                        port: existing.port,
                        http_port: desired_public_ports.http_port,
                        https_port: desired_public_ports.https_port,
                        description: desired_description,
                    },
                )?;
                servers.save()?;
                output::success(&format!(
                    "Updated server {} (tako@{}:{})",
                    output::strong(&server_name),
                    host,
                    port
                ));
                record_server_history(host, &server_name, port);
                return Ok(Some(server_name));
            }

            output::success(&format!(
                "Server {} is already configured (tako@{}:{})",
                output::strong(&server_name),
                host,
                port
            ));
            record_server_history(host, &server_name, port);
            return Ok(Some(server_name));
        }

        let confirm = output::confirm(
            &format!(
                "Host {} already exists as {}. Override?",
                output::strong(host),
                output::strong(&existing_name)
            ),
            false,
        )?;

        if !confirm {
            output::operation_cancelled();
            return Ok(None);
        }

        servers.remove(&existing_name)?;
    }

    // Check if name already exists (with different host)
    if servers.contains(&server_name) {
        return Err(format!(
            "Server name '{}' already exists. Use --name to specify a different name.",
            server_name
        )
        .into());
    }

    if output::is_dry_run() {
        output::dry_run_skip(&format!(
            "Add server {} (tako@{}:{})",
            output::strong(&server_name),
            host,
            port
        ));
        return Ok(Some(server_name));
    }

    let mut detected_target: Option<ServerTarget> = pre_detected_target;
    let should_verify_access = install_if_missing || !no_test || detected_target.is_some();
    if should_verify_access {
        output::with_spinner_async_simple("Checking server", verify_tailscale_host(host)).await?;
    }

    // Test SSH connection unless skipped or already tested
    if (!no_test || install_if_missing) && detected_target.is_none() {
        let mut result = output::with_spinner_async_err(
            "Connecting",
            "Connection successful",
            "Connection failed",
            check_tako_connection(host, port),
        )
        .await;

        let needs_install = match &result {
            Ok(info) => !info.installed,
            Err(_) => true,
        };
        if install_if_missing && needs_install {
            let admin_user = admin_user.unwrap_or("root");
            let install_ports = install_public_ports(public_ports)?;
            let first_run_settings = super::first_run::prompt_first_run_settings()?;
            output::with_spinner_async_err(
                "Installing tako-server",
                "tako-server installed",
                "Install failed",
                install_tako_server_with_admin(
                    host,
                    port,
                    admin_user,
                    Some(install_ports),
                    crate::ssh::InstallServerMode::BootstrapOnly,
                ),
            )
            .await?;
            resolved_public_ports = Some(install_ports);
            apply_first_run_settings_and_start(host, port, install_ports, &first_run_settings)
                .await?;

            result = output::with_spinner_async_err(
                "Verifying install",
                "Install verified",
                "Verification failed",
                check_tako_connection(host, port),
            )
            .await;
        } else if allow_install_prompt && needs_install && output::is_interactive() {
            let should_install = output::confirm("Install tako-server now?", true)?;
            if should_install {
                let install_ports = install_public_ports(public_ports)?;
                let admin_user = output::TextField::new("Admin SSH user")
                    .with_default(admin_user.unwrap_or("root"))
                    .prompt()?;
                let first_run_settings = super::first_run::prompt_first_run_settings()?;
                output::with_spinner_async_err(
                    "Installing tako-server",
                    "tako-server installed",
                    "Install failed",
                    install_tako_server_with_admin(
                        host,
                        port,
                        &admin_user,
                        Some(install_ports),
                        crate::ssh::InstallServerMode::BootstrapOnly,
                    ),
                )
                .await?;
                resolved_public_ports = Some(install_ports);
                apply_first_run_settings_and_start(host, port, install_ports, &first_run_settings)
                    .await?;

                result = output::with_spinner_async_err(
                    "Verifying install",
                    "Install verified",
                    "Verification failed",
                    check_tako_connection(host, port),
                )
                .await;
            }
        }

        let needs_configure = matches!(
            &result,
            Ok(info) if info.installed && info.public_ports.is_none()
        );
        if needs_configure
            && (install_if_missing || (allow_install_prompt && output::is_interactive()))
        {
            let should_configure = if install_if_missing {
                true
            } else {
                output::confirm("Set up and start tako-server now?", true)?
            };
            if should_configure {
                let configure_ports = install_public_ports(public_ports)?;
                let first_run_settings = super::first_run::prompt_first_run_settings()?;
                apply_first_run_settings_and_start(
                    host,
                    port,
                    configure_ports,
                    &first_run_settings,
                )
                .await?;
                resolved_public_ports = Some(configure_ports);

                result = output::with_spinner_async_err(
                    "Verifying server",
                    "Server verified",
                    "Verification failed",
                    check_tako_connection(host, port),
                )
                .await;
            }
        }

        {
            let _host_scope = output::scope(host).entered();
            match result {
                Ok(info) => {
                    tracing::debug!("Target: {}", info.target.label());
                    if let Some(ref ver) = info.version {
                        let ver = ver.strip_prefix("tako-server ").unwrap_or(ver);
                        tracing::debug!("Server version: {ver}");
                    }
                    if !info.installed {
                        return Err(server_not_installed_message().into());
                    }
                    detected_target = Some(info.target);
                    resolved_public_ports = info.public_ports.or(resolved_public_ports);
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
    } else if detected_target.is_none() {
        output::warning(
            "Skipped SSH test. Target metadata was not detected; deploy will fail for this server until it is re-added with SSH checks enabled.",
        );
    }

    if should_verify_access {
        let probe =
            output::with_spinner_async_simple("Checking server", verify_remote_management(host))
                .await?;
        resolved_public_ports = Some(ServerPublicPorts {
            http_port: probe.info.http_port,
            https_port: probe.info.https_port,
        });
        trace_management_probe(host, &probe);
    }

    // Add the server
    let resolved_public_ports = resolved_public_ports.unwrap_or_default();
    let entry = ServerEntry {
        host: host.to_string(),
        port,
        http_port: resolved_public_ports.http_port,
        https_port: resolved_public_ports.https_port,
        description: normalized_description.clone(),
    };

    servers.add(server_name.clone(), entry)?;
    if let Some(target) = detected_target {
        servers.set_target(&server_name, target)?;
    }
    servers.save()?;

    output::success(&format!("Added server {}", output::strong(&server_name),));
    record_server_history(host, &server_name, port);

    Ok(Some(server_name))
}

fn server_not_installed_message() -> &'static str {
    "tako-server is not installed. Run `tako servers add <admin-user>@<host>` or install it on the server, then try again."
}

#[cfg(test)]
mod tests;
