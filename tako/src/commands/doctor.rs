mod common;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(all(test, target_os = "macos"))]
mod tests;

use common::{format_apps, format_certificate, format_dev_server, format_paths, gather_ca_status};
#[cfg(target_os = "linux")]
use linux::{format_linux_dns, format_linux_sections, gather_linux_data};
#[cfg(target_os = "macos")]
use macos::{format_local_dns, format_macos_sections, gather_macos_data};

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
