mod server;
mod system;

use std::path::PathBuf;
use std::process::Command;

use crate::output;
pub use server::implode_server;
use system::{
    gather_system_targets, has_ca_certs_in_keychain, remove_ca_certs_from_keychain,
    remove_system_targets,
};

pub fn run(assume_yes: bool) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(assume_yes))
}

async fn run_async(assume_yes: bool) -> Result<(), Box<dyn std::error::Error>> {
    let user_targets = gather_user_targets()?;
    let system_targets = gather_system_targets();
    let has_ca_certs = has_ca_certs_in_keychain();

    if user_targets.is_empty() && system_targets.is_empty() && !has_ca_certs {
        output::muted("Nothing to remove — Tako does not appear to be installed.");
        return Ok(());
    }

    output::warning("This will permanently remove Tako and all local data:");
    eprintln!();
    for target in &user_targets {
        output::muted(&format!("  {}", target.display()));
    }
    if !system_targets.is_empty() || has_ca_certs {
        output::muted("  System services and config (requires sudo):");
        for desc in &system_targets {
            output::muted(&format!("    {}", desc.description));
        }
        if has_ca_certs {
            output::muted("    CA certificate(s) in system keychain");
        }
    }
    eprintln!();

    if !assume_yes {
        let confirmed = output::confirm("Remove Tako and all local data?", false)?;
        if !confirmed {
            output::operation_cancelled();
            return Ok(());
        }
    }

    // Best-effort: stop dev server before removing data
    let _ = stop_dev_server().await;

    // Remove system-level items first (requires sudo)
    if !system_targets.is_empty() || has_ca_certs {
        output::warning("Sudo is required to remove system-level components.");
        let sudo_status = Command::new("sudo")
            .arg("-v")
            .status()
            .map_err(|e| format!("failed to run sudo: {e}"))?;
        if sudo_status.success() {
            remove_system_targets(&system_targets);
            if has_ca_certs {
                remove_ca_certs_from_keychain();
            }
        } else {
            output::error("Sudo authentication failed — skipping system-level cleanup");
        }
    }

    // Remove user-level items (directories + binaries)
    let mut errors = Vec::new();
    for target in &user_targets {
        if !target.exists() {
            continue;
        }
        let result = if target.is_dir() {
            std::fs::remove_dir_all(target)
        } else {
            std::fs::remove_file(target)
        };
        match result {
            Ok(()) => output::success(&format!("Removed {}", target.display())),
            Err(e) => {
                output::error(&format!("Failed to remove {}: {e}", target.display()));
                errors.push(e);
            }
        }
    }

    if errors.is_empty() {
        eprintln!();
        output::success("Tako has been removed");
    } else {
        eprintln!();
        output::warning(&format!(
            "Tako partially removed ({} item(s) could not be deleted)",
            errors.len()
        ));
    }

    Ok(())
}

/// Collect user-level paths (no sudo needed).
fn gather_user_targets() -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let config_dir = crate::paths::tako_config_dir()?;
    let data_dir = crate::paths::tako_data_dir()?;
    let binaries = find_tako_binaries();

    Ok(gather_user_targets_from(config_dir, data_dir, binaries))
}

fn gather_user_targets_from(
    config_dir: PathBuf,
    data_dir: PathBuf,
    binaries: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut targets = Vec::new();

    if config_dir.exists() {
        targets.push(config_dir.clone());
    }
    if data_dir.exists() && data_dir != config_dir {
        targets.push(data_dir);
    }
    for bin in binaries {
        targets.push(bin);
    }

    targets
}

/// Find Tako binaries in the same directory as the running executable.
fn find_tako_binaries() -> Vec<PathBuf> {
    let Ok(exe) = std::env::current_exe() else {
        return vec![];
    };
    let Some(dir) = exe.parent() else {
        return vec![];
    };

    ["tako", "tako-dev-server", "tako-dev-proxy"]
        .iter()
        .map(|name| dir.join(name))
        .filter(|path| path.exists())
        .collect()
}

async fn stop_dev_server() -> Result<(), Box<dyn std::error::Error>> {
    let apps = crate::dev_server_client::list_registered_apps().await?;
    for app in &apps {
        let _ = crate::dev_server_client::unregister_app(&app.config_path).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
