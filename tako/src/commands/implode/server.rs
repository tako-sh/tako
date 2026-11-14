use crate::output;

pub async fn implode_server(
    server_name: &str,
    server: &crate::config::ServerEntry,
    assume_yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::ssh::SshClient;

    if output::is_dry_run() {
        if server.native.is_some() {
            output::dry_run_skip(&format!(
                "Revoke network membership and delegated approvals for {server_name}"
            ));
        }
        output::dry_run_skip(&format!("Remove tako-server and all data on {server_name}"));
        return Ok(());
    }

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
    if server.native.is_some() {
        output::muted("  Network:   revoke membership and delegated approvals");
    }
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

    uninstall_remote_with(
        server,
        async { Ok(SshClient::connect_to(server).await?) },
        |target| {
            output::with_spinner(
                "Revoking network membership and delegated approvals",
                "Recorded network revocation",
                || {
                    tokio::task::block_in_place(|| {
                        crate::network::revocation::run(
                            &target.network,
                            &target.server_id.to_string(),
                        )?;
                        Ok(())
                    })
                },
            )
        },
        async |ssh| {
            output::with_spinner_async(
                &format!("Removing tako-server from {server_name}"),
                &format!("Removed tako-server from {server_name}"),
                async { ssh.exec_checked(&build_server_implode_command()).await },
            )
            .await?;
            Ok(())
        },
    )
    .await?;

    if server.native.is_some() {
        output::hint("Offline peers enforce the revocation when they reconnect.");
    }

    Ok(())
}

/// Keep SSH available until revocation is durably recorded, then remove the service.
async fn uninstall_remote_with<C>(
    server: &crate::config::ServerEntry,
    connect: impl std::future::Future<Output = Result<C, Box<dyn std::error::Error>>>,
    revoke: impl FnOnce(&crate::management_http::NativeTarget) -> Result<(), Box<dyn std::error::Error>>,
    remove: impl AsyncFnOnce(C) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let connection = connect.await?;
    if let Some(target) = &server.native {
        revoke(target).map_err(|error| {
            format!(
                "Uninstall incomplete: could not confirm network revocation; server files have not been removed. {error}"
            )
        })?;
    }
    remove(connection).await.map_err(|error| {
        if server.native.is_some() {
            format!(
                "Uninstall incomplete: network membership is revoked, but remote cleanup failed. Remove the remaining service and data over SSH: {error}"
            )
            .into()
        } else {
            error
        }
    })
}

pub(super) fn build_server_implode_command() -> String {
    crate::ssh::SshClient::run_as_root("/usr/local/bin/tako-server-service implode")
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
