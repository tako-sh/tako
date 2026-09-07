use crate::output;

pub async fn implode_server(
    server_name: &str,
    server: &crate::config::ServerEntry,
    assume_yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::ssh::SshClient;

    output::warning(&format!(
        "This will permanently remove tako-server and all data on {}",
        output::strong(server_name),
    ));
    eprintln!();
    output::muted("  Services:  tako-server, tako-server-standby");
    output::muted(
        "  Binaries:  /usr/local/bin/tako-server, tako-server-service, tako-server-install-refresh",
    );
    output::muted("  Data:      /opt/tako/");
    output::muted("  Sockets:   /var/run/tako/");
    output::muted("  Service files (systemd)");
    eprintln!();

    if !assume_yes {
        let confirmed = output::confirm(
            &format!(
                "Remove tako-server and all data on {}?",
                output::strong(server_name)
            ),
            false,
        )?;
        if !confirmed {
            output::operation_cancelled();
            return Ok(());
        }
    }

    let ssh = SshClient::connect_to(server).await?;

    let cmd = build_server_implode_command();

    output::with_spinner_async(
        &format!("Removing tako-server from {server_name}"),
        &format!("Removed tako-server from {server_name}"),
        async { ssh.exec_checked(&cmd).await },
    )
    .await?;

    // Remove server from local config
    let mut servers = crate::config::ServersToml::load()?;
    servers.remove(server_name)?;
    servers.save()?;

    output::success(&format!(
        "Removed {} from local server list",
        output::strong(server_name)
    ));

    Ok(())
}

pub(super) fn build_server_implode_command() -> String {
    crate::ssh::SshClient::run_as_root("/usr/local/bin/tako-server-service implode")
}
