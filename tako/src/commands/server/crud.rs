use crate::output;
use std::time::Duration;

fn removal_option_label(name: &str, entry: &crate::config::ServerEntry) -> String {
    match entry.description.as_deref().map(str::trim) {
        Some(description) if !description.is_empty() => {
            format!("{name} ({description})  {}:{}", entry.host, entry.port)
        }
        _ => format!("{name}  {}:{}", entry.host, entry.port),
    }
}

pub(super) async fn remove_server(name: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    use crate::config::ServersToml;

    let mut servers = ServersToml::load()?;

    if servers.is_empty() {
        output::error("No servers configured.");
        output::hint(&format!(
            "Run {} to add a server.",
            output::strong("tako servers add")
        ));
        return Ok(());
    }

    if let Some(name) = name {
        if !servers.contains(name) {
            return Err(format!("Server '{}' not found.", name).into());
        }

        if output::is_dry_run() {
            output::dry_run_skip(&format!("Remove server {}", output::strong(name)));
            return Ok(());
        }

        let confirm = output::confirm(&format!("Remove {}?", output::strong(name)), false)?;

        if !confirm {
            output::operation_cancelled();
            return Ok(());
        }

        servers.remove(name)?;
        servers.save()?;

        output::success(&format!("Removed {}", output::strong(name)));
        return Ok(());
    }

    if !output::is_interactive() {
        return Err(
            "No server name provided and selection requires an interactive terminal. Run 'tako servers remove <name>'."
                .into(),
        );
    }

    let mut step = 0;
    let mut selected_name = String::new();

    loop {
        match step {
            // Step 0: Select server
            0 => {
                let mut names = servers.names();
                names.sort_unstable();
                let options: Vec<(String, String)> = names
                    .into_iter()
                    .filter_map(|server_name| {
                        servers.get(server_name).map(|entry| {
                            (
                                removal_option_label(server_name, entry),
                                server_name.to_string(),
                            )
                        })
                    })
                    .collect();

                match output::select("Select server to remove", None, options) {
                    Ok(name) => {
                        output::muted(&format!("Server: {name}"));
                        selected_name = name;
                        step = 1;
                    }
                    Err(e) if output::is_wizard_back(&e) => return Ok(()),
                    Err(e) => return Err(e.into()),
                }
            }
            // Step 1: Confirm
            1 => {
                match output::confirm(
                    &format!("Remove {}?", output::strong(&selected_name)),
                    false,
                ) {
                    Ok(true) => {
                        servers.remove(&selected_name)?;
                        servers.save()?;
                        output::success(&format!("Removed {}", output::strong(&selected_name)));
                        return Ok(());
                    }
                    Ok(false) => {
                        output::operation_cancelled();
                        return Ok(());
                    }
                    Err(e) if output::is_wizard_back(&e) => {
                        step = 0;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            _ => unreachable!(),
        }
    }
}

pub(super) async fn list_servers() -> Result<(), Box<dyn std::error::Error>> {
    use crate::config::ServersToml;

    let servers = ServersToml::load()?;

    if servers.is_empty() {
        tracing::warn!("No servers configured");
        output::warning("No servers configured");
        output::hint(&format!(
            "Run {} to add a server.",
            output::strong("tako servers add")
        ));
        return Ok(());
    }

    let mut names = servers.names();
    names.sort_unstable();

    for name in &names {
        let entry = match servers.get(name) {
            Some(e) => e,
            None => continue,
        };

        let header = if entry.port != 22 {
            format!("{} ({}:{})", output::strong(name), entry.host, entry.port)
        } else {
            format!("{} ({})", output::strong(name), entry.host)
        };
        let _scope = output::scope(name).entered();
        tracing::info!("Server listed ({}:{})", entry.host, entry.port);
        output::info(&header);

        if let Some(desc) = entry
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
        {
            output::bullet(&format!("{} {desc}", output::theme_muted("Description")));
        }

        if entry.http_port != 80 || entry.https_port != 443 {
            output::bullet(&format!(
                "{} HTTP {}, HTTPS {}",
                output::theme_muted("Public ports"),
                entry.http_port,
                entry.https_port
            ));
        }
    }
    Ok(())
}

pub(super) async fn restart_server(
    name: &str,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::config::ServersToml;
    use crate::ssh::{SshClient, SshConfig};

    let servers = ServersToml::load()?;

    let server = servers
        .get(name)
        .ok_or_else(|| format!("Server '{}' not found.", name))?;

    let _scope = output::scope(name).entered();
    let ssh_config = SshConfig::from_server(&server.host, server.port);
    let mut ssh = SshClient::new(ssh_config);
    let _t = output::timed("SSH connected");
    output::with_spinner_async(&format!("Connecting to {name}"), "Connected", ssh.connect())
        .await?;
    drop(_t);

    if force {
        let _t = output::timed("Force restart tako-server");
        match output::with_spinner_async(
            "Force restarting tako-server",
            "tako-server restarted",
            ssh.tako_restart(),
        )
        .await
        {
            Ok(()) => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                match ssh.tako_status().await {
                    Ok(status) => {
                        if status == "active" {
                            output::success("tako-server is running");
                        } else {
                            output::warning(&format!("tako-server status: {}", status));
                        }
                    }
                    Err(e) => {
                        output::warning(&format!("Could not check status: {}", e));
                    }
                }
            }
            Err(e) => {
                drop(_t);
                output::error(&format!("Force restart failed: {}", e));
                ssh.disconnect().await?;
                return Err(format!("Failed to force restart tako-server: {}", e).into());
            }
        }
        drop(_t);
    } else {
        let old_pid = ssh
            .tako_server_info()
            .await
            .map_err(|e| format!("Failed to read runtime config: {e}"))?
            .pid;

        let _t = output::timed("Reload tako-server");
        match output::with_spinner_async(
            "Reloading tako-server",
            "tako-server reloaded",
            ssh.tako_reload(),
        )
        .await
        {
            Ok(()) => {
                if let Err(e) = super::upgrade::wait_for_primary_ready(
                    &mut ssh,
                    super::upgrade::UPGRADE_SOCKET_WAIT_TIMEOUT,
                    old_pid,
                    name,
                )
                .await
                {
                    drop(_t);
                    output::error(&format!("Reload failed: {}", e));
                    ssh.disconnect().await?;
                    return Err(format!("Failed to reload tako-server: {}", e).into());
                }
                output::success("tako-server is running");
            }
            Err(e) => {
                drop(_t);
                output::error(&format!("Reload failed: {}", e));
                ssh.disconnect().await?;
                return Err(format!("Failed to reload tako-server: {}", e).into());
            }
        }
        drop(_t);
    }

    ssh.disconnect().await?;

    Ok(())
}
