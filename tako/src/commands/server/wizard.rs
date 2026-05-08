use crate::output;
use tracing::Instrument;

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

    run_add_server_wizard(None, None, 22, true, true, None).await
}

fn append_unique_suggestions(target: &mut Vec<String>, source: &[String]) {
    for value in source {
        push_unique_suggestion(target, value.clone());
    }
}

fn push_unique_suggestion(values: &mut Vec<String>, value: String) {
    if value.is_empty() {
        return;
    }
    if values.iter().any(|existing| existing == &value) {
        return;
    }
    values.push(value);
}

struct WizardConnectionResult {
    target: crate::config::ServerTarget,
    version: Option<String>,
    installed: bool,
    server_name: Option<String>,
}

pub struct AddServerOptions<'a> {
    pub name: Option<&'a str>,
    pub description: Option<&'a str>,
    pub port: u16,
    pub no_test: bool,
    pub pre_detected_target: Option<crate::config::ServerTarget>,
    pub install_if_missing: bool,
    pub allow_install_prompt: bool,
    pub admin_user: Option<&'a str>,
}

async fn check_tako_connection(host: &str, port: u16) -> Result<WizardConnectionResult, String> {
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

async fn install_tako_server_with_admin(
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

pub(super) async fn run_add_server_wizard(
    initial_name: Option<&str>,
    initial_description: Option<&str>,
    initial_port: u16,
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

    // --- SSH connection test ---
    let mut remote_server_name: Option<String> = None;
    let mut detected_target: Option<crate::config::ServerTarget> = None;

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
                let admin_user = output::TextField::new("Admin SSH user")
                    .with_default(admin_user_default.unwrap_or("root"))
                    .prompt()?;
                let install_scope = output::scope(&host);
                let _t = output::timed(&format!("Install tako-server on {host}:{port}"));
                output::with_spinner_async_err(
                    "Installing tako-server",
                    "tako-server installed",
                    "Install failed",
                    install_tako_server_with_admin(&host, port, &admin_user)
                        .instrument(install_scope),
                )
                .await?;
                drop(_t);

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
            no_test: true,
            pre_detected_target: detected_target,
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
        no_test,
        pre_detected_target,
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

    // Check if host already exists
    if let Some(existing_name) = servers.find_by_host(host) {
        let existing_name = existing_name.to_string();
        let existing = servers
            .get(&existing_name)
            .cloned()
            .ok_or_else(|| format!("Server '{}' vanished during lookup", existing_name))?;

        if existing_name == server_name && existing.port == port {
            if normalized_description.is_some()
                && existing.description.as_deref() != normalized_description.as_deref()
            {
                servers.update(
                    &existing_name,
                    ServerEntry {
                        host: existing.host,
                        port: existing.port,
                        description: normalized_description.clone(),
                    },
                )?;
                servers.save()?;
                output::success(&format!(
                    "Updated description for server {} (tako@{}:{})",
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
        output::with_spinner_async_err(
            "Checking Tailscale",
            "Tailscale ready",
            "Tailscale required",
            verify_tailscale_host(host),
        )
        .await?;
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
            output::with_spinner_async_err(
                "Installing tako-server",
                "tako-server installed",
                "Install failed",
                install_tako_server_with_admin(host, port, admin_user),
            )
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
                let admin_user = output::TextField::new("Admin SSH user")
                    .with_default(admin_user.unwrap_or("root"))
                    .prompt()?;
                output::with_spinner_async_err(
                    "Installing tako-server",
                    "tako-server installed",
                    "Install failed",
                    install_tako_server_with_admin(host, port, &admin_user),
                )
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
        let probe = output::with_spinner_async_err(
            "Checking server access",
            "Server access verified",
            "Server access failed",
            verify_remote_management(host),
        )
        .await?;
        trace_management_probe(host, &probe);
    }

    // Add the server
    let entry = ServerEntry {
        host: host.to_string(),
        port,
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

fn default_server_name_from_host(host: &str) -> Option<String> {
    let label = host
        .trim()
        .trim_end_matches('.')
        .split('.')
        .next()
        .unwrap_or("")
        .trim();
    if is_valid_default_server_name(label) {
        Some(label.to_string())
    } else {
        None
    }
}

fn next_available_server_name(base: &str, servers: &crate::config::ServersToml) -> String {
    for index in 2.. {
        let suffix = format!("-{index}");
        let max_base_len = 63usize.saturating_sub(suffix.len());
        let trimmed_base = base
            .chars()
            .take(max_base_len)
            .collect::<String>()
            .trim_end_matches('-')
            .to_string();
        let candidate = format!("{trimmed_base}{suffix}");
        if !servers.contains(&candidate) {
            return candidate;
        }
    }

    unreachable!()
}

fn is_valid_default_server_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 63 || name.ends_with('-') {
        return false;
    }
    name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

async fn verify_remote_management(
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

async fn verify_tailscale_host(host: &str) -> Result<(), String> {
    crate::tailscale::ensure_tailscale_host(host)
        .await
        .map_err(|_| remote_management_unavailable_message())
}

fn remote_management_unavailable_message() -> String {
    format!(
        "{} Connect this machine and the server to Tailscale, then run `tako servers add` with the server's MagicDNS name.",
        crate::tailscale::required_message()
    )
}

fn trace_management_probe(host: &str, probe: &crate::management_http::ManagementProbe) {
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

pub(super) async fn detect_server_target(
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

fn parse_detected_arch(stdout: &str) -> Result<String, String> {
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

fn parse_detected_libc(stdout: &str) -> Result<String, String> {
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

fn record_server_history(host: &str, name: &str, port: u16) {
    let mut history = crate::config::CliHistoryToml::load().unwrap_or_default();
    history.record_server_prompt_values(host, name, port);
    if let Err(e) = history.save() {
        tracing::warn!("Could not save CLI history: {e}");
    }
}

#[cfg(test)]
mod tests;
