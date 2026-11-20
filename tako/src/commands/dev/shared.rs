use std::path::Path;

use crate::config::TakoToml;

pub(crate) fn load_dev_tako_toml(config_path: &Path) -> crate::config::Result<TakoToml> {
    TakoToml::load_from_file(config_path)
}

pub(crate) fn validate_dev_environment(config: &TakoToml, environment: &str) -> Result<(), String> {
    crate::config::validate_environment_name(environment).map_err(|e| e.to_string())?;
    if environment != "development"
        && !config.envs.contains_key(environment)
        && !config.vars_per_env.contains_key(environment)
        && !config.compose.as_ref().is_some_and(|compose| {
            compose
                .services
                .values()
                .any(|service| service.envs.contains_key(environment))
        })
    {
        return Err(format!("Environment '{environment}' is not configured"));
    }
    Ok(())
}

/// The daemon treats this as an opaque registration key, not a filesystem path.
pub(crate) fn dev_config_key(config_path: &Path, environment: &str) -> String {
    let config_path = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    if environment == "development" {
        config_path.display().to_string()
    } else {
        format!("{environment}:{}", config_path.display())
    }
}

pub(crate) fn port_from_listen(listen: &str) -> Option<u16> {
    listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
}

pub(crate) fn restart_required_for_requested_listen(
    existing_listen: Option<&str>,
    requested_listen: &str,
) -> bool {
    match existing_listen {
        Some(current) => current != requested_listen,
        None => false,
    }
}

#[cfg(test)]
pub(crate) fn doctor_dev_server_lines(
    listen: &str,
    port: u64,
    local_443_forwarding: bool,
    local_80_forwarding: bool,
    local_dns_enabled: bool,
    local_dns_port: u16,
) -> Vec<String> {
    let mut lines = vec!["dev-server:".to_string(), format!("  listen: {}", listen)];

    let port_is_duplicate = u16::try_from(port)
        .ok()
        .zip(port_from_listen(listen))
        .is_some_and(|(reported, from_listen)| reported == from_listen);
    if port > 0 && !port_is_duplicate {
        lines.push(format!("  port: {}", port));
    }

    lines.push(format!("  local_443_forwarding: {}", local_443_forwarding));
    lines.push(format!("  local_80_forwarding: {}", local_80_forwarding));
    lines.push(format!("  local_dns_enabled: {}", local_dns_enabled));
    lines.push(format!("  local_dns_port: {}", local_dns_port));
    lines
}

#[cfg(test)]
pub(crate) fn doctor_local_forwarding_preflight_lines(
    advertised_ip: &str,
    proxy_loaded: bool,
    https_tcp_ok: bool,
    http_tcp_ok: bool,
) -> Vec<String> {
    vec![
        "preflight:".to_string(),
        format!(
            "- dev proxy ({})",
            if proxy_loaded { "loaded" } else { "not loaded" }
        ),
        format!(
            "- TCP {}:443 ({})",
            advertised_ip,
            if https_tcp_ok { "ok" } else { "unreachable" }
        ),
        format!(
            "- TCP {}:80 ({})",
            advertised_ip,
            if http_tcp_ok { "ok" } else { "unreachable" }
        ),
    ]
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn system_resolver_ipv4(hostname: &str) -> Option<String> {
    use std::net::ToSocketAddrs;

    (hostname, 443)
        .to_socket_addrs()
        .ok()?
        .find_map(|addr| match addr.ip() {
            std::net::IpAddr::V4(v4) => Some(v4.to_string()),
            std::net::IpAddr::V6(_) => None,
        })
}
