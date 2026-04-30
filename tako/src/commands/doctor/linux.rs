use crate::output;

use super::common::{format_active_status, heading, hinted_row, label_width};

#[cfg(target_os = "linux")]
pub(super) struct LinuxData {
    status: super::super::dev::LinuxSetupStatus,
    advertised_ip: String,
    host_dns_results: Vec<(String, Option<String>)>,
}

#[cfg(target_os = "linux")]
pub(super) fn gather_linux_data(
    _dev_info: &Result<serde_json::Value, Box<dyn std::error::Error>>,
    apps: &[crate::dev_server_client::ListedApp],
) -> LinuxData {
    use super::super::dev::{DEV_LOOPBACK_ADDR, system_resolver_ipv4};

    let status = super::super::dev::linux_setup_status();
    let advertised_ip = DEV_LOOPBACK_ADDR.to_string();

    let host_dns_results: Vec<(String, Option<String>)> = apps
        .iter()
        .flat_map(|a| {
            if a.hosts.is_empty() {
                vec![crate::dev::get_tako_domain(&a.app_name)]
            } else {
                a.hosts.clone()
            }
        })
        .filter(|h| h.ends_with(".tako.test"))
        .map(|host| {
            let ip = system_resolver_ipv4(&host);
            (host, ip)
        })
        .collect();

    LinuxData {
        status,
        advertised_ip,
        host_dns_results,
    }
}

#[cfg(target_os = "linux")]
pub(super) fn format_linux_sections(buf: &mut Vec<String>, linux: &LinuxData) {
    heading(buf, "Port Redirect");
    let fwd_width = label_width(&[
        "Alias",
        "TCP 443 redirect",
        "TCP 80 redirect",
        "UDP 53 redirect",
        "Persistence",
    ]);

    hinted_row(
        buf,
        "Alias",
        &format_active_status(linux.status.loopback_alias, "ok", "missing"),
        fwd_width,
        "127.77.0.1 is assigned on the lo loopback interface",
    );
    hinted_row(
        buf,
        "TCP 443 redirect",
        &format_active_status(linux.status.redirect_443, "ok", "missing"),
        fwd_width,
        "iptables redirects 127.77.0.1:443 to the dev server HTTPS port",
    );
    hinted_row(
        buf,
        "TCP 80 redirect",
        &format_active_status(linux.status.redirect_80, "ok", "missing"),
        fwd_width,
        "iptables redirects 127.77.0.1:80 to the dev server HTTP port",
    );
    hinted_row(
        buf,
        "UDP 53 redirect",
        &format_active_status(linux.status.redirect_dns, "ok", "missing"),
        fwd_width,
        "iptables redirects 127.77.0.1:53 to the dev server DNS port",
    );
    hinted_row(
        buf,
        "Persistence",
        &format_active_status(linux.status.service_installed, "installed", "not installed"),
        fwd_width,
        "systemd oneshot service restores redirect rules at boot",
    );
}

#[cfg(target_os = "linux")]
pub(super) fn format_linux_dns(
    buf: &mut Vec<String>,
    _dev_info: &Result<serde_json::Value, Box<dyn std::error::Error>>,
    _apps: &[crate::dev_server_client::ListedApp],
    linux: &LinuxData,
) {
    heading(buf, "Local DNS");

    let mut dns_labels: Vec<&str> = vec!["Resolved config"];
    let host_strs: Vec<&str> = linux
        .host_dns_results
        .iter()
        .map(|(h, _)| h.as_str())
        .collect();
    dns_labels.extend_from_slice(&host_strs);
    let dns_w = label_width(&dns_labels);

    let resolved_status = if linux.status.dns_configured {
        output::theme_success("configured")
    } else {
        output::theme_warning("not configured")
    };
    hinted_row(
        buf,
        "Resolved config",
        &resolved_status,
        dns_w,
        "systemd-resolved drop-in that routes *.tako.test to the local DNS server",
    );

    for (host, ip) in &linux.host_dns_results {
        match ip {
            Some(ip) if ip == &linux.advertised_ip => {
                hinted_row(
                    buf,
                    host,
                    &format!("{} {}", output::theme_muted("→"), ip),
                    dns_w,
                    "Current system DNS answer for this app hostname",
                );
            }
            Some(ip) => {
                hinted_row(
                    buf,
                    host,
                    &format!(
                        "{} {} {}",
                        output::theme_muted("→"),
                        ip,
                        output::theme_warning(format!("(expected {})", linux.advertised_ip))
                    ),
                    dns_w,
                    "Current system DNS answer for this app hostname",
                );
            }
            None => {
                hinted_row(
                    buf,
                    host,
                    &format!(
                        "{} {}",
                        output::theme_muted("→"),
                        output::theme_warning("no answer")
                    ),
                    dns_w,
                    "Current system DNS answer for this app hostname",
                );
            }
        }
    }
}
