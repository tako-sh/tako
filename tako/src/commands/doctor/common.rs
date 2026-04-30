use crate::output;

pub(super) fn heading(buf: &mut Vec<String>, title: &str) {
    if output::is_pretty() {
        buf.push(String::new());
        buf.push(output::strong(title));
    }
}

pub(super) fn label_width(labels: &[&str]) -> usize {
    labels.iter().map(|s| s.len()).max().unwrap_or(0)
}

pub(super) fn row(buf: &mut Vec<String>, label: &str, value: &str, width: usize) {
    let padding = width.saturating_sub(label.len());
    buf.push(format!("  {}{}  {}", label, " ".repeat(padding), value,));
}

pub(super) fn hint(buf: &mut Vec<String>, text: &str) {
    buf.push(format!("    {}", output::theme_muted(text)));
}

pub(super) fn hinted_row(
    buf: &mut Vec<String>,
    label: &str,
    value: &str,
    width: usize,
    text: &str,
) {
    row(buf, label, value, width);
    hint(buf, text);
}

pub(super) fn format_bool_status(enabled: bool) -> String {
    if enabled {
        output::theme_success("enabled")
    } else {
        output::theme_warning("disabled")
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn format_active_status(ok: bool, ok_label: &str, fail_label: &str) -> String {
    if ok {
        output::theme_success(ok_label)
    } else {
        output::theme_error(fail_label)
    }
}

// ─── Data gathering ──────────────────────────────────────────────────────────

pub(super) enum CaStatus {
    Error(String),
    NotCreated,
    Trusted,
    Untrusted,
}

pub(super) fn gather_ca_status() -> CaStatus {
    let store = match crate::dev::LocalCAStore::new() {
        Ok(s) => s,
        Err(e) => return CaStatus::Error(e.to_string()),
    };
    if !store.ca_exists() {
        CaStatus::NotCreated
    } else if store.is_ca_trusted() {
        CaStatus::Trusted
    } else {
        CaStatus::Untrusted
    }
}

pub(super) fn format_paths(buf: &mut Vec<String>, config_dir: &str, data_dir: &str) {
    heading(buf, "Paths");
    let w = label_width(&["Config", "Data"]);
    hinted_row(
        buf,
        "Config",
        config_dir,
        w,
        "Directory where Tako stores local configuration files",
    );
    hinted_row(
        buf,
        "Data",
        data_dir,
        w,
        "Directory where Tako stores runtime state and cached assets",
    );
}

pub(super) fn format_certificate(buf: &mut Vec<String>, status: &CaStatus) {
    heading(buf, "Local CA");
    let w = label_width(&["Status"]);
    let value = match status {
        CaStatus::Error(e) => output::theme_error(format!("error: {e}")),
        CaStatus::NotCreated => output::theme_warning("not created"),
        CaStatus::Trusted => output::theme_success("trusted"),
        CaStatus::Untrusted => output::theme_warning("untrusted"),
    };
    hinted_row(
        buf,
        "Status",
        &value,
        w,
        "Trust state of the Tako local certificate authority for https://*.tako.test",
    );
}

pub(super) fn format_dev_server(
    buf: &mut Vec<String>,
    dev_info: &Result<serde_json::Value, Box<dyn std::error::Error>>,
) {
    use super::super::dev::{LOCAL_DNS_PORT, is_dev_server_unavailable_error_message};

    heading(buf, "Development server");

    let w = label_width(&["Status", "Listen", "Port", "Local DNS", "Local DNS port"]);

    let info = match dev_info {
        Ok(info) => info,
        Err(e) => {
            let message = e.to_string();
            let status = if is_dev_server_unavailable_error_message(&message) {
                output::theme_warning("not running")
            } else {
                output::theme_error(format!("error: {e}"))
            };
            hinted_row(
                buf,
                "Status",
                &status,
                w,
                "Current health of the local Tako development server process",
            );
            return;
        }
    };

    let i = info.get("info").unwrap_or(&serde_json::Value::Null);
    let listen = i
        .get("listen")
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)");
    let port = i.get("port").and_then(|v| v.as_u64()).unwrap_or(0);
    let local_dns_enabled = i
        .get("local_dns_enabled")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let local_dns_port = i
        .get("local_dns_port")
        .and_then(|v| v.as_u64())
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(LOCAL_DNS_PORT);

    hinted_row(
        buf,
        "Listen",
        listen,
        w,
        "Address where the Tako development server listens for local proxy traffic",
    );

    let port_is_duplicate = u16::try_from(port)
        .ok()
        .zip(super::super::dev::port_from_listen(listen))
        .is_some_and(|(reported, from_listen)| reported == from_listen);
    if port > 0 && !port_is_duplicate {
        hinted_row(
            buf,
            "Port",
            &port.to_string(),
            w,
            "Public HTTPS port currently reported by the Tako development server",
        );
    }

    hinted_row(
        buf,
        "Local DNS",
        &format_bool_status(local_dns_enabled),
        w,
        "Whether the Tako development server has its local DNS responder enabled",
    );
    hinted_row(
        buf,
        "Local DNS port",
        &local_dns_port.to_string(),
        w,
        "UDP port used by the local Tako DNS responder",
    );
}
pub(super) fn format_apps(buf: &mut Vec<String>, apps: &[crate::dev_server_client::ListedApp]) {
    if apps.is_empty() {
        return;
    }
    heading(buf, "Apps");
    for a in apps {
        let hosts = if a.hosts.is_empty() {
            "(default)".to_string()
        } else {
            a.hosts.join(", ")
        };
        let pid_str = a
            .pid
            .map(|p| format!("  {}", output::theme_muted(format!("pid {p}"))))
            .unwrap_or_default();
        buf.push(format!(
            "  {}  {}  port {}{}",
            output::strong(&a.app_name),
            output::theme_muted(&hosts),
            a.upstream_port,
            pid_str,
        ));
    }
}
