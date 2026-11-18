use crate::config::{ServerEntry, ServerInventory};
use crate::output;

/// What `tako network server remove` leaves on the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Removal {
    /// Delete every app, its data, and the server's network identity, keeping
    /// Tako installed so the machine can be added to a network again.
    Reset,
    /// Also remove tako-server, its helpers, and its service files.
    Uninstall,
}

impl Removal {
    fn helper_action(self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::Uninstall => "implode",
        }
    }
}

pub(super) async fn remove_server(
    name: Option<&str>,
    network: Option<&str>,
    removal: Removal,
    assume_yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let servers = crate::commands::helpers::load_management_servers(None, None).await?;
    if servers.is_empty() {
        return Err("No servers configured. Run 'tako network server add' to add one.".into());
    }

    let networks = crate::config::LocalNetworks::load()?;
    let server_name = match name {
        Some(name) => {
            crate::commands::helpers::resolve_server_selector(&networks, &servers, name, network)?
        }
        None => {
            if !output::is_interactive() {
                return Err(
                    "No server name provided and selection requires an interactive terminal. Run 'tako network server remove <name>'."
                        .into(),
                );
            }
            let options: Vec<(String, String)> =
                crate::commands::helpers::server_choices(&networks, &servers, network)?
                    .into_iter()
                    .map(|name| (name.clone(), name))
                    .collect();
            output::select("Select server to remove", None, options)?
        }
    };
    let server = servers
        .get(&server_name)
        .ok_or_else(|| format!("Server '{server_name}' not found."))?
        .clone();
    let network_name = server
        .native
        .as_ref()
        .map(|target| target.network.name.clone());

    if is_last_server(&servers, &server) {
        let network = network_name.as_deref().unwrap_or_default();
        if !assume_yes {
            return Err(format!(
                "{server_name} is the last server in network {network}. Removing it leaves nothing to deploy to and no server to recover client access from. Pass --yes to remove it anyway."
            )
            .into());
        }
        output::warning(&format!(
            "{} is the last server in network {network}",
            output::strong(&server_name)
        ));
    }

    let apps = server_apps(&server).await;
    let listed = match &apps {
        Ok(apps) if apps.is_empty() => "none".to_string(),
        Ok(apps) => apps.join(", "),
        Err(error) => format!("could not list ({error})"),
    };
    let summary = serde_json::json!({
        "network": network_name,
        "server": server_name,
        "apps": apps.as_ref().ok(),
        "uninstalled": removal == Removal::Uninstall,
    });

    if output::is_dry_run() {
        if server.native.is_some() {
            output::dry_run_skip(&format!("Revoke network membership for {server_name}"));
        }
        output::dry_run_skip(&format!("Delete every app and its data on {server_name}"));
        if removal == Removal::Uninstall {
            output::dry_run_skip(&format!("Remove tako-server from {server_name}"));
        }
        if output::is_json() {
            let mut summary = summary;
            summary["dry_run"] = true.into();
            output::json_result(summary)?;
        }
        return Ok(());
    }

    output::warning(&format!(
        "This permanently deletes every app and its data on {}",
        output::strong(&server_name),
    ));
    eprintln!();
    output::muted(&format!("  Apps:      {listed}"));
    output::muted(
        "  Data:      /opt/tako/ (releases, volumes, databases, backups, network identity)",
    );
    if let Some(network) = &network_name {
        output::muted(&format!("  Network:   revoke membership in {network}"));
    }
    if removal == Removal::Uninstall {
        output::muted("  Services:  tako-server, tako-server-standby, tako-network-agent");
        output::muted(
            "  Binaries:  /usr/local/bin/tako-server, tako-server-service, tako-server-install-refresh",
        );
    }
    eprintln!();

    if !assume_yes {
        if !output::is_interactive() {
            return Err(format!("Pass --yes to remove {server_name}.").into());
        }
        let question = match removal {
            Removal::Reset => format!(
                "Remove {} and delete its apps?",
                output::strong(&server_name)
            ),
            Removal::Uninstall => format!(
                "Remove {}, delete its apps, and uninstall Tako?",
                output::strong(&server_name)
            ),
        };
        if !output::confirm(&question, false)? {
            output::operation_cancelled();
            return Ok(());
        }
    }

    remove_remote_with(
        &server,
        removal,
        async { Ok(crate::ssh::SshClient::connect_to(&server).await?) },
        |target| {
            output::with_spinner(
                "Revoking network membership",
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
            let (loading, done) = match removal {
                Removal::Reset => (
                    format!("Deleting apps and data on {server_name}"),
                    format!("Deleted apps and data on {server_name}"),
                ),
                Removal::Uninstall => (
                    format!("Removing tako-server from {server_name}"),
                    format!("Removed tako-server from {server_name}"),
                ),
            };
            output::with_spinner_async(&loading, &done, async {
                ssh.exec_checked(&build_helper_command(removal)).await
            })
            .await?;
            Ok(())
        },
    )
    .await?;

    if let Some(target) = &server.native {
        tokio::task::block_in_place(|| {
            if let Err(error) = crate::network::state::pull(&target.network, None) {
                output::hint(&format!(
                    "Membership changed; refresh network state with `tako network sync`: {error}"
                ));
            }
        });
        output::hint("Offline peers enforce the revocation when they reconnect.");
    }
    if removal == Removal::Reset {
        output::hint(&format!(
            "Tako stays installed. Run {} to add the machine to a network again.",
            output::strong("tako network server add")
        ));
    }
    if output::is_json() {
        output::json_result(summary)?;
    }
    Ok(())
}

/// Whether `server` is the only server of its network.
fn is_last_server(servers: &ServerInventory, server: &ServerEntry) -> bool {
    let Some(target) = &server.native else {
        return false;
    };
    !servers.servers.values().any(|entry| {
        entry.native.as_ref().is_some_and(|other| {
            other.network.id == target.network.id && other.server_id != target.server_id
        })
    })
}

/// The app environments deployed on `server`, sorted.
async fn server_apps(server: &ServerEntry) -> Result<Vec<String>, String> {
    let mut client = crate::management_http::ManagementClient::for_server(server)
        .await
        .map_err(|error| error.to_string())?;
    let response = client
        .send(&tako_core::Command::List)
        .await
        .map_err(|error| error.to_string())?;
    crate::commands::delete::parse_list_apps_response(response)
}

/// Keep SSH available until revocation is durably recorded, then clean up the server.
async fn remove_remote_with<C>(
    server: &ServerEntry,
    removal: Removal,
    connect: impl std::future::Future<Output = Result<C, Box<dyn std::error::Error>>>,
    revoke: impl FnOnce(&crate::management_http::NativeTarget) -> Result<(), Box<dyn std::error::Error>>,
    remove: impl AsyncFnOnce(C) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let connection = connect.await?;
    if let Some(target) = &server.native {
        revoke(target).map_err(|error| {
            format!(
                "Removal incomplete: could not confirm network revocation; nothing on the server was deleted. {error}"
            )
        })?;
    }
    remove(connection).await.map_err(|error| {
        if server.native.is_some() {
            format!(
                "Removal incomplete: network membership is revoked, but cleanup on the server failed. Finish it over SSH with `sudo /usr/local/bin/tako-server-service {}`: {error}",
                removal.helper_action()
            )
            .into()
        } else {
            error
        }
    })
}

fn build_helper_command(removal: Removal) -> String {
    crate::ssh::SshClient::run_as_root(&format!(
        "/usr/local/bin/tako-server-service {}",
        removal.helper_action()
    ))
}

#[cfg(test)]
#[path = "remove_tests.rs"]
mod tests;
