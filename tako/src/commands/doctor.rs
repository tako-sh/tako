mod common;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(all(test, target_os = "macos"))]
mod tests;

use common::{
    CaStatus, format_apps, format_certificate, format_dev_server, format_paths, gather_ca_status,
};
#[cfg(target_os = "linux")]
use linux::{format_linux_dns, format_linux_sections, gather_linux_data};
#[cfg(target_os = "macos")]
use macos::{format_local_dns, format_macos_sections, gather_macos_data};
use serde_json::{Value, json};

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // ── Gather all data upfront ──────────────────────────────────────────

    let config_dir = crate::paths::tako_config_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(error)".into());
    let data_dir = crate::paths::tako_data_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(error)".into());

    let ca_status = gather_ca_status();

    let dev_info = crate::dev_server_client::info().await;
    let apps = crate::dev_server_client::list_apps()
        .await
        .unwrap_or_default();

    #[cfg(target_os = "macos")]
    let macos_data = gather_macos_data(&dev_info, &apps);
    #[cfg(target_os = "linux")]
    let linux_data = gather_linux_data(&dev_info, &apps);

    if crate::output::is_json() {
        return crate::output::json_result(json_report(
            &config_dir,
            &data_dir,
            &ca_status,
            &dev_info,
            &apps,
            #[cfg(target_os = "macos")]
            &macos_data,
            #[cfg(target_os = "linux")]
            &linux_data,
        ));
    }

    // ── Format output ────────────────────────────────────────────────────

    let mut buf = Vec::new();

    format_paths(&mut buf, &config_dir, &data_dir);
    format_certificate(&mut buf, &ca_status);
    format_dev_server(&mut buf, &dev_info);

    #[cfg(target_os = "macos")]
    format_macos_sections(&mut buf, &dev_info, &apps, &macos_data);
    #[cfg(target_os = "linux")]
    format_linux_sections(&mut buf, &linux_data);

    format_apps(&mut buf, &apps);

    #[cfg(target_os = "macos")]
    format_local_dns(&mut buf, &dev_info, &apps, &macos_data);
    #[cfg(target_os = "linux")]
    format_linux_dns(&mut buf, &dev_info, &apps, &linux_data);

    for line in &buf {
        eprintln!("{line}");
    }

    Ok(())
}

fn json_report(
    config_dir: &str,
    data_dir: &str,
    ca_status: &CaStatus,
    dev_info: &Result<Value, Box<dyn std::error::Error>>,
    apps: &[crate::dev_server_client::ListedApp],
    #[cfg(target_os = "macos")] macos_data: &macos::MacosData,
    #[cfg(target_os = "linux")] linux_data: &linux::LinuxData,
) -> Value {
    let mut report = json!({
        "ok": true,
        "command": "doctor",
        "paths": {
            "config": config_dir,
            "data": data_dir,
        },
        "ca": json_ca(ca_status),
        "dev_server": json_dev_server(dev_info),
        "apps": apps.iter().map(|app| json!({
            "name": app.app_name,
            "variant": app.variant,
            "hosts": app.hosts,
            "port": app.upstream_port,
            "pid": app.pid,
        })).collect::<Vec<_>>(),
    });

    #[cfg(target_os = "macos")]
    {
        report["proxy"] = macos_data.json_value();
    }
    #[cfg(target_os = "linux")]
    {
        report["proxy"] = linux_data.json_value();
    }

    report
}

fn json_ca(status: &CaStatus) -> Value {
    match status {
        CaStatus::Error(message) => json!({ "status": "error", "message": message }),
        CaStatus::NotCreated => json!({ "status": "not_created" }),
        CaStatus::Trusted => json!({ "status": "trusted" }),
        CaStatus::Untrusted => json!({ "status": "untrusted" }),
    }
}

fn json_dev_server(dev_info: &Result<Value, Box<dyn std::error::Error>>) -> Value {
    match dev_info {
        Ok(info) => {
            let i = info.get("info").unwrap_or(&Value::Null);
            json!({
                "status": "running",
                "listen": i.get("listen"),
                "port": i.get("port"),
                "local_dns": i.get("local_dns_enabled"),
                "local_dns_port": i.get("local_dns_port"),
            })
        }
        Err(error) => {
            let message = error.to_string();
            if super::dev::is_dev_server_unavailable_error_message(&message) {
                json!({ "status": "not_running" })
            } else {
                json!({ "status": "error", "message": message })
            }
        }
    }
}
