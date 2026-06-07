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
    output::muted("  Service files (systemd/OpenRC)");
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

    let ssh = SshClient::connect_to(&server.host, server.port).await?;

    let script = build_server_implode_script();
    let cmd = SshClient::run_with_root_or_sudo(&script);

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

pub(super) fn build_server_implode_script() -> String {
    // Stop and disable services (supports both systemd and OpenRC)
    // Remove service files, binaries, data, and sockets
    [
        // Stop services
        "if command -v systemctl >/dev/null 2>&1; then",
        "  systemctl stop tako-server tako-server-standby 2>/dev/null || true",
        "  systemctl disable tako-server tako-server-standby 2>/dev/null || true",
        "fi",
        "if command -v rc-service >/dev/null 2>&1; then",
        "  rc-service tako-server stop 2>/dev/null || true",
        "  rc-service tako-server-standby stop 2>/dev/null || true",
        "  rc-update del tako-server 2>/dev/null || true",
        "  rc-update del tako-server-standby 2>/dev/null || true",
        "fi",
        // Remove systemd service files and drop-ins
        "rm -f /etc/systemd/system/tako-server.service",
        "rm -f /etc/systemd/system/tako-server-standby.service",
        "rm -rf /etc/systemd/system/tako-server.service.d",
        "if command -v systemctl >/dev/null 2>&1; then systemctl daemon-reload 2>/dev/null || true; fi",
        // Remove OpenRC service files
        "rm -f /etc/init.d/tako-server",
        "rm -f /etc/init.d/tako-server-standby",
        // Remove binaries
        "rm -f /usr/local/bin/tako-server",
        "rm -f /usr/local/bin/tako-server-service",
        "rm -f /usr/local/bin/tako-server-install-refresh",
        // Remove data and sockets
        "rm -rf /opt/tako",
        "rm -rf /var/run/tako",
    ]
    .join("\n")
}
