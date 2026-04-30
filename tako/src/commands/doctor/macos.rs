use crate::output;

use super::common::{format_active_status, heading, hinted_row, label_width};

pub(super) struct MacosData {
    pub(super) dev_proxy: super::super::dev::DevProxyStatus,
    pub(super) https_tcp_ok: bool,
    pub(super) http_tcp_ok: bool,
    pub(super) advertised_ip: String,
    pub(super) local_dns_port: u16,
    pub(super) resolver_values: Option<(String, u16)>,
    pub(super) host_dns_results: Vec<(String, Option<String>)>,
}

#[cfg(target_os = "macos")]
pub(super) fn gather_macos_data(
    dev_info: &Result<serde_json::Value, Box<dyn std::error::Error>>,
    apps: &[crate::dev_server_client::ListedApp],
) -> MacosData {
    use super::super::dev::{
        DEV_LOOPBACK_ADDR, LOCAL_DNS_PORT, dev_proxy_status, local_dns_resolver_values,
        system_resolver_ipv4,
    };

    let local_dns_port = match dev_info {
        Ok(info) => {
            let i = info.get("info").unwrap_or(&serde_json::Value::Null);
            i.get("local_dns_port")
                .and_then(|v| v.as_u64())
                .and_then(|v| u16::try_from(v).ok())
                .unwrap_or(LOCAL_DNS_PORT)
        }
        Err(_) => LOCAL_DNS_PORT,
    };

    let dev_proxy = dev_proxy_status();
    let advertised_ip = DEV_LOOPBACK_ADDR.to_string();
    let resolver_values = local_dns_resolver_values();

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

    MacosData {
        https_tcp_ok: dev_proxy.https_ready,
        http_tcp_ok: dev_proxy.http_ready,
        dev_proxy,
        advertised_ip,
        local_dns_port,
        resolver_values,
        host_dns_results,
    }
}

pub(super) fn format_macos_sections(
    buf: &mut Vec<String>,
    _dev_info: &Result<serde_json::Value, Box<dyn std::error::Error>>,
    _apps: &[crate::dev_server_client::ListedApp],
    macos: &MacosData,
) {
    let tcp_443 = format!("TCP {}:443", macos.advertised_ip);
    let tcp_80 = format!("TCP {}:80", macos.advertised_ip);
    let fwd_width = label_width(&[
        "Installed",
        "Boot Helper",
        "Alias",
        "Launchd",
        &tcp_443,
        &tcp_80,
    ]);

    heading(buf, "Dev Proxy");
    hinted_row(
        buf,
        "Installed",
        &format_active_status(macos.dev_proxy.installed, "ok", "missing"),
        fwd_width,
        "Binary and support files are present on disk",
    );
    hinted_row(
        buf,
        "Boot Helper",
        &format_active_status(macos.dev_proxy.bootstrap_loaded, "loaded", "not loaded"),
        fwd_width,
        "Boot-time helper is loaded so Tako can restore dev proxy setup",
    );
    hinted_row(
        buf,
        "Alias",
        &format_active_status(macos.dev_proxy.alias_ready, "ok", "missing"),
        fwd_width,
        "127.77.0.1 is assigned on the lo0 loopback interface",
    );
    hinted_row(
        buf,
        "Launchd",
        &format_active_status(macos.dev_proxy.launchd_loaded, "loaded", "not loaded"),
        fwd_width,
        "macOS launchd has loaded the proxy service definition",
    );
    hinted_row(
        buf,
        &tcp_443,
        &format_active_status(macos.https_tcp_ok, "ok", "unreachable"),
        fwd_width,
        "HTTPS proxy is listening on the loopback address and accepts connections",
    );
    hinted_row(
        buf,
        &tcp_80,
        &format_active_status(macos.http_tcp_ok, "ok", "unreachable"),
        fwd_width,
        "HTTP proxy is listening on the loopback address and accepts connections",
    );
}

pub(super) fn format_local_dns(
    buf: &mut Vec<String>,
    _dev_info: &Result<serde_json::Value, Box<dyn std::error::Error>>,
    _apps: &[crate::dev_server_client::ListedApp],
    macos: &MacosData,
) {
    use super::super::dev::TAKO_RESOLVER_FILE;

    heading(buf, "Local DNS");

    let mut dns_labels: Vec<&str> = vec!["Resolver"];
    let host_strs: Vec<&str> = macos
        .host_dns_results
        .iter()
        .map(|(h, _)| h.as_str())
        .collect();
    dns_labels.extend_from_slice(&host_strs);
    let dns_w = label_width(&dns_labels);

    match &macos.resolver_values {
        Some((nameserver, port)) if nameserver == "127.0.0.1" && *port == macos.local_dns_port => {
            hinted_row(
                buf,
                "Resolver",
                &format!(
                    "{} {} {}",
                    TAKO_RESOLVER_FILE,
                    output::theme_muted("→"),
                    format_args!("{nameserver}:{port}")
                ),
                dns_w,
                "Resolver file that should direct *.tako.test lookups to the local DNS server",
            );
        }
        Some((nameserver, port)) => {
            hinted_row(
                buf,
                "Resolver",
                &format!(
                    "{} {} {} {}",
                    TAKO_RESOLVER_FILE,
                    output::theme_muted("→"),
                    format_args!("{nameserver}:{port}"),
                    output::theme_warning(format!("(expected 127.0.0.1:{})", macos.local_dns_port))
                ),
                dns_w,
                "Resolver file that should direct *.tako.test lookups to the local DNS server",
            );
        }
        None => {
            hinted_row(
                buf,
                "Resolver",
                &format!(
                    "{} {} {}",
                    TAKO_RESOLVER_FILE,
                    output::theme_muted("→"),
                    output::theme_warning("missing")
                ),
                dns_w,
                "Resolver file that should direct *.tako.test lookups to the local DNS server",
            );
        }
    }

    for (host, ip) in &macos.host_dns_results {
        match ip {
            Some(ip) if ip == &macos.advertised_ip => {
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
                        output::theme_warning(format!("(expected {})", macos.advertised_ip))
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
