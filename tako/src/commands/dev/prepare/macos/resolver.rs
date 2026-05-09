#[cfg(target_os = "macos")]
use std::path::Path;

#[cfg(target_os = "macos")]
use super::super::super::{RESOLVER_DIR, SHORT_RESOLVER_FILE, TAKO_RESOLVER_FILE};
#[cfg(target_os = "macos")]
use super::{pending_sudo_action, sudo_run_checked, write_system_file_with_sudo};

#[cfg(any(target_os = "macos", test))]
pub(crate) fn local_dns_resolver_contents(port: u16) -> String {
    format!("nameserver 127.0.0.1\nport {port}\n")
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn parse_local_dns_resolver(contents: &str) -> (Option<String>, Option<u16>) {
    let mut nameserver = None;
    let mut port = None;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        let key = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or_default();
        match key {
            "nameserver" if !value.is_empty() => nameserver = Some(value.to_string()),
            "port" => {
                if let Ok(v) = value.parse::<u16>() {
                    port = Some(v);
                }
            }
            _ => {}
        }
    }

    (nameserver, port)
}

#[cfg(target_os = "macos")]
fn resolver_file_matches(path: &str, port: u16) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let (nameserver, configured_port) = parse_local_dns_resolver(&contents);
    nameserver.as_deref() == Some("127.0.0.1") && configured_port == Some(port)
}

#[cfg(target_os = "macos")]
fn local_dns_resolver_configured(port: u16) -> bool {
    resolver_file_matches(TAKO_RESOLVER_FILE, port)
}

#[cfg(target_os = "macos")]
fn short_dns_resolver_configured(port: u16) -> bool {
    resolver_file_matches(SHORT_RESOLVER_FILE, port)
}

#[cfg(target_os = "macos")]
pub(crate) fn ensure_local_dns_resolver_configured(
    port: u16,
) -> Result<bool, Box<dyn std::error::Error>> {
    let tako_ok = local_dns_resolver_configured(port);
    let short_ok = short_dns_resolver_configured(port);

    if tako_ok && short_ok {
        return Ok(true);
    }

    if !tako_ok && !crate::output::is_interactive() && !crate::output::is_root() {
        return Err(format!(
            "local DNS resolver is not configured at {TAKO_RESOLVER_FILE}; run `tako dev` interactively once to install it"
        )
        .into());
    }

    sudo_run_checked(
        &["install", "-d", "-m", "755", RESOLVER_DIR],
        "creating /etc/resolver",
    )?;

    if !tako_ok {
        write_system_file_with_sudo(TAKO_RESOLVER_FILE, &local_dns_resolver_contents(port))?;

        if !local_dns_resolver_configured(port) {
            return Err("local DNS resolver setup verification failed".into());
        }
    }

    let short_active = if short_ok {
        true
    } else if !Path::new(SHORT_RESOLVER_FILE).exists() {
        write_system_file_with_sudo(SHORT_RESOLVER_FILE, &local_dns_resolver_contents(port))?;
        short_dns_resolver_configured(port)
    } else if crate::output::is_interactive() {
        crate::output::warning(
            "Another tool owns /etc/resolver/test. Override it for shorter *.test URLs?",
        );
        if crate::output::confirm("Override /etc/resolver/test?", false).unwrap_or(false) {
            write_system_file_with_sudo(SHORT_RESOLVER_FILE, &local_dns_resolver_contents(port))?;
            short_dns_resolver_configured(port)
        } else {
            crate::output::muted("Skipped — using *.tako.test URLs instead.");
            false
        }
    } else {
        false
    };

    Ok(short_active)
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn local_dns_sudo_action_line() -> &'static str {
    "Configure DNS"
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn sudo_setup_action_items(
    ca_action: Option<&str>,
    local_dns_needed: bool,
    dev_proxy_action: Option<&str>,
) -> Vec<String> {
    let mut items = Vec::new();
    if let Some(action) = ca_action {
        items.push(action.to_string());
    }
    if local_dns_needed {
        items.push(local_dns_sudo_action_line().to_string());
    }
    if let Some(action) = dev_proxy_action {
        items.push(action.to_string());
    }
    items
}

#[cfg(target_os = "macos")]
pub(crate) fn explain_pending_sudo_setup(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    if !crate::output::is_interactive() {
        return Ok(());
    }

    let dns_needed = !local_dns_resolver_configured(port) || !short_dns_resolver_configured(port);
    let items = sudo_setup_action_items(
        super::super::tls::pending_sudo_action()?,
        dns_needed,
        pending_sudo_action()?,
    );
    if items.is_empty() {
        return Ok(());
    }

    crate::output::warning("sudo access required");
    if crate::output::is_pretty() {
        eprintln!("Tako needs this to set up your development environment:");
        for item in items {
            eprintln!("- {item}");
        }
    }
    eprintln!();

    let status = std::process::Command::new("sudo")
        .arg("-v")
        .status()
        .map_err(|e| -> Box<dyn std::error::Error> { format!("failed to run sudo: {e}").into() })?;
    if !status.success() {
        return Err(crate::output::silent_exit_error().into());
    }

    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn local_dns_resolver_values() -> Option<(String, u16)> {
    let contents = std::fs::read_to_string(TAKO_RESOLVER_FILE).ok()?;
    let (nameserver, port) = parse_local_dns_resolver(&contents);
    Some((nameserver?, port?))
}
