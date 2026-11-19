mod client;

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use tako_network::{
    Approval, DeviceKind, DeviceName, LocalDeviceKeys, MemberKind, NetworkDescriptor, NetworkId,
    Permissions, RegistrySnapshot, WgChallenge,
};

use crate::config::LocalNetworks;
use crate::network::{self, Profile, Result};
use crate::output;

#[derive(Subcommand)]
pub enum NetworkCommands {
    /// Refresh membership and the local cache of encrypted secrets
    Sync { network: Option<String> },
    /// Manage servers in a network
    #[command(subcommand)]
    Server(super::server::ServerCommands),
    /// Invite, inspect, and revoke clients
    #[command(subcommand)]
    Client(client::ClientCommands),
    /// Join a network with a single-use invitation or a client identity
    Join {
        /// Read the invitation or identity from standard input instead of the
        /// protected prompt
        #[arg(long)]
        invitation_stdin: bool,
        /// This machine's name for the network (asked when omitted)
        #[arg(long)]
        alias: Option<String>,
    },
    /// Create a private network with this machine as its first full client
    Create {
        /// The network's shared name
        name: String,
        /// This machine's name for the network (defaults to its name)
        #[arg(long)]
        alias: Option<String>,
    },
    /// Refresh and inspect network membership
    Status { network: Option<String> },
    /// Export public network configuration without private keys or app secrets
    Export {
        network: Option<String>,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
    /// List this machine's networks with their aliases and stable IDs
    #[command(visible_alias = "ls")]
    List,
    /// Change this machine's alias for a network
    Rename {
        /// Current alias or stable ID
        network: String,
        /// New alias
        alias: String,
    },
}

pub fn run(command: NetworkCommands) -> Result<()> {
    match command {
        NetworkCommands::Sync { network: value } => {
            let alias = network::resolve_network(value.as_deref())?;
            if !output::is_dry_run() {
                network::state::pull(&alias, None)?;
            }
            finish(
                "Synchronized network state",
                serde_json::json!({"network":alias.alias,"dry_run":output::is_dry_run()}),
            )
        }
        NetworkCommands::Server(command) => super::server::run(command),
        NetworkCommands::Client(command) => client::run(command),
        NetworkCommands::Join {
            invitation_stdin,
            alias,
        } => client::join(invitation_stdin, alias.as_deref()),
        NetworkCommands::Create { name, alias } => create(&name, alias.as_deref()),
        NetworkCommands::Export { network, output } => export(network.as_deref(), &output),
        NetworkCommands::List => list(),
        NetworkCommands::Rename { network, alias } => {
            if output::is_dry_run() {
                return finish(
                    "Would rename the network on this machine",
                    serde_json::json!({"dry_run": true, "network": network, "alias": alias}),
                );
            }
            let renamed = LocalNetworks::rename(&network, &alias)?;
            finish(
                &format!("Renamed network {network} to {}", renamed.alias),
                serde_json::to_value(&renamed)?,
            )
        }
        NetworkCommands::Status { network } => status(network.as_deref()),
    }
}

/// Servers only peer with full clients, so a deploy client has nothing to join.
fn require_full_client(profile: &Profile) -> Result<()> {
    let own = profile
        .request()
        .device_id_for(&profile.snapshot().network.id)?;
    if !profile.snapshot().is_full(&own, network::now()?)? {
        return Err(
            "Deploy clients cannot join the private network; they manage apps over HTTPS only."
                .into(),
        );
    }
    Ok(())
}

/// Full access unless the client is restricted to deploying some scopes.
pub(super) fn access(deploy: Vec<tako_network::AppEnv>) -> Permissions {
    if deploy.is_empty() {
        Permissions::Full
    } else {
        Permissions::deploy(deploy)
    }
}

pub(super) fn manage_server(
    device: &str,
    value: Option<&str>,
    command: tako_core::Command,
) -> Result<()> {
    use crate::management_http::{ManagementClient, ManagementTarget, NativeTarget, parse_ok_data};
    let alias = network::resolve_network(value)?;
    let profile = if output::is_dry_run() {
        network::load_profile(&alias)?
    } else {
        network::client::refresh(&alias)?
    };
    let members = network::server_members(&profile)?;
    let candidates: Vec<_> = members
        .iter()
        .filter(|(id, member)| id.to_string() == device || member.request.name.as_str() == device)
        .collect();
    let [(id, member)] = candidates.as_slice() else {
        return Err("Server must uniquely match a name or device ID".into());
    };
    if output::is_dry_run() {
        return finish(
            "Would update server drain state",
            serde_json::json!({"dry_run": true, "network": alias.alias, "device": id, "command": command}),
        );
    }
    let target = ManagementTarget::Native(NativeTarget {
        network: alias.clone(),
        server_id: (*id).clone(),
        server_name: member.request.name.clone(),
    });
    let runtime = tokio::runtime::Runtime::new()?;
    let result: serde_json::Value = runtime.block_on(async {
        let mut client = ManagementClient::for_target(&target).await?;
        parse_ok_data(client.send(&command).await?, "server drain")
    })?;
    if matches!(command, tako_core::Command::DrainServer { .. }) {
        if result.get("completed").and_then(serde_json::Value::as_bool) == Some(true) {
            output::success("Server drained; network management remains available");
        } else {
            output::hint(
                "Drain remains in progress. The server rejects new application traffic; run this command again to check and finish cleanup.",
            );
        }
        if result.get("timed_out").and_then(serde_json::Value::as_bool) == Some(true) {
            output::hint(
                "The drain deadline expired; remaining processes were asked to stop. Review the drain report before removing the server.",
            );
        }
        output::hint(
            "Drain does not move databases, volumes, or external ingress. Revoke the server only after its work and data are accounted for.",
        );
    } else {
        output::success("Server resumed");
    }
    if output::is_json() {
        output::json_result(
            serde_json::json!({"network": alias.alias, "device": id, "result": result}),
        )?;
    }
    Ok(())
}

fn create(name: &str, alias: Option<&str>) -> Result<()> {
    crate::config::validate_network_name(name)?;
    let networks = LocalNetworks::load()?;
    let alias = choose_alias(&networks, name, alias, "create")?;
    if output::is_dry_run() {
        return finish(
            "Would create a network and local device keys",
            serde_json::json!({"dry_run": true, "name": name, "alias": alias}),
        );
    }
    let id = NetworkId::random()?;
    let keys = LocalDeviceKeys::generate()?;
    let client = device_name(None)?;
    let request = keys.enrollment_request(Some(id.clone()), DeviceKind::Laptop, client.clone())?;
    let now = network::now()?;
    let (challenge, pending) = WgChallenge::begin(&request, id.clone(), now, now + 60)?;
    let verified = pending.verify(&keys.answer_challenge(&challenge, &request, now)?, now)?;
    let mut snapshot = RegistrySnapshot {
        network: NetworkDescriptor {
            name: name.parse()?,
            id,
            initial_admin: verified.device_id().clone(),
        },
        members: Vec::new(),
        revocations: Vec::new(),
        acks: Vec::new(),
    };
    snapshot.members.push(snapshot.approve(
        verified,
        Approval {
            kind: MemberKind::Laptop,
            permissions: Permissions::Full,
            approved_by: None,
            independent_root: false,
            created_at: now,
            expires_at: None,
            client: Some(client),
        },
    )?);
    network::store_keys(&request.request_id, &keys)?;
    let local = network::save_profile_as(&Profile::Created { request, snapshot }, Some(&alias))?;
    finish(
        &format!("Created network {}", network::network_label(&local)),
        serde_json::to_value(&local)?,
    )?;
    output::hint(&format!(
        "Add a server with `tako network server add <admin@host> --network {}`.",
        local.alias
    ));
    Ok(())
}

/// This machine's alias for a network named `name` that it is about to
/// create or join. An explicit alias must be free. Otherwise a join asks,
/// suggesting the name or a numbered variant, and a create uses the name when
/// it is free. Without a terminal, a taken name fails instead of quietly
/// becoming `name-2`, so a script's next `--network name` cannot reach the
/// older network.
pub(super) fn choose_alias(
    networks: &LocalNetworks,
    name: &str,
    alias: Option<&str>,
    action: &str,
) -> Result<String> {
    let free = |alias: &str| -> Result<()> {
        crate::config::validate_network_alias(alias)?;
        match networks.find(alias) {
            Some(other) => Err(format!(
                "Alias '{alias}' already names network {} on this machine. Pass --alias with another name.",
                other.id
            )
            .into()),
            None => Ok(()),
        }
    };
    if let Some(alias) = alias {
        free(alias)?;
        return Ok(alias.to_string());
    }
    let suggested = networks.free_alias(name);
    if action == "create" && suggested == name {
        return Ok(suggested);
    }
    if !output::is_interactive() {
        free(name)?;
        return Ok(name.to_string());
    }
    let alias = output::TextField::new("Alias for this network")
        .with_hint("Commands on this machine name the network by its alias.")
        .with_default(&suggested)
        .prompt_validated(|value| free(value.trim()).map_err(|error| error.to_string()))?;
    Ok(alias.trim().to_string())
}

fn export(value: Option<&str>, destination: &Path) -> Result<()> {
    let alias = network::resolve_network(value)?;
    let profile = network::load_profile(&alias)?;
    if output::is_dry_run() {
        return finish(
            "Would export the public network profile",
            serde_json::json!({"dry_run": true, "output": destination}),
        );
    }
    network::write_json(destination, &profile, false)?;
    finish(
        "Exported public network profile",
        serde_json::json!({"output": destination, "id": alias.id}),
    )?;
    output::hint(
        "A new machine needs its own enrollment request. This export contains no private keys or app secrets.",
    );
    Ok(())
}

fn list() -> Result<()> {
    let networks = LocalNetworks::load()?;
    let mut listed: Vec<_> = networks.networks.values().collect();
    listed.sort_by(|a, b| a.alias.cmp(&b.alias));
    if output::is_json() {
        return output::json_result(serde_json::json!({ "networks": listed }));
    }
    for network in listed {
        let line = network::network_label(network);
        output::info(&line);
        tracing::info!("{line}");
    }
    if networks.networks.is_empty() {
        output::hint("No networks configured. Run `tako network create <name>`.");
    }
    Ok(())
}

fn device_name(name: Option<&str>) -> Result<DeviceName> {
    if let Some(name) = name {
        return Ok(name.parse()?);
    }
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            let result = std::process::Command::new("hostname").output().ok()?;
            result
                .status
                .success()
                .then(|| String::from_utf8_lossy(&result.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "laptop".into());
    Ok(hostname.split('.').next().unwrap_or("laptop").parse()?)
}

fn finish(message: &str, value: serde_json::Value) -> Result<()> {
    output::success(message);
    if output::is_json() {
        output::json_result(value)?;
    }
    Ok(())
}

pub fn connect(value: Option<&str>) -> Result<()> {
    let alias = network::resolve_network(value)?;
    if output::is_dry_run() {
        let profile = network::load_profile(&alias)?;
        require_full_client(&profile)?;
        return finish(
            "Would connect to the network",
            serde_json::json!({"dry_run": true, "network": alias.alias}),
        );
    }
    let profile = network::client::refresh(&alias)?;
    require_full_client(&profile)?;
    // One network owns `.tako` names at a time, so connecting switches. Hold a
    // machine-wide lock so concurrent connects cannot both find nothing to replace.
    let _switch = switch_lock()?;
    // Check the new network first so a rejected switch keeps the current connection.
    let others: Vec<_> = connected_networks()?
        .into_iter()
        .filter(|(other, _)| other.id != alias.id)
        .collect();
    let replacing: Vec<&str> = others.iter().map(|(other, _)| other.id.as_str()).collect();
    let _lifecycle = network::wireguard::lifecycle_lock(&alias)?;
    network::local_access::preflight(&alias, &profile, &replacing)?;
    // Teardown and setup can both fail after the preflight. Bring back what the
    // switch disconnected, including a partly torn-down network, rather than
    // leaving no network connected.
    for (index, (other, other_profile)) in others.iter().enumerate() {
        if let Err(error) = teardown(other, other_profile) {
            restore_all(&others[..=index]);
            return Err(error);
        }
        output::muted(&format!("Disconnected from {}", other.alias));
    }
    if let Err(error) = network::wireguard::connect(&alias, &profile) {
        restore_all(&others);
        return Err(error);
    }
    network::local_access::install(&alias, &profile).map_err(|error| {
        format!("The network is connected, but SSH aliases could not be installed: {error}")
    })?;
    let unreachable = {
        let _t = output::timed("Check WireGuard handshakes");
        output::with_spinner_silent("Checking servers", || {
            network::wireguard::unreachable_servers(&profile, HANDSHAKE_WAIT)
        })
    };
    output::success(&format!("Connected to {}", alias.alias));
    let (unreachable, check_error) = match unreachable {
        Ok(servers) => (servers, None),
        Err(error) => {
            let message = format!("Could not check server reachability: {error}");
            output::warning(&message);
            tracing::warn!("{message}");
            (Vec::new(), Some(error.to_string()))
        }
    };
    for server in &unreachable {
        if let MemberKind::Server(info) = &server.kind {
            // `warning` is pretty-only; tracing keeps it in --ci and --verbose transcripts.
            let message = format!(
                "No WireGuard handshake with {}. Check that UDP {} reaches {}.",
                server.request.name,
                info.wireguard_endpoint.port(),
                info.wireguard_endpoint.ip()
            );
            output::warning(&message);
            tracing::warn!("{message}");
        }
    }
    if output::is_json() {
        let unreachable: Vec<_> = unreachable
            .iter()
            .map(|server| server.request.name.to_string())
            .collect();
        // Without a completed check, report reachability as unknown rather
        // than an empty list that would read as "every server answered".
        output::json_result(serde_json::json!({
            "network": alias.alias,
            "connected": true,
            "reachability_checked": check_error.is_none(),
            "reachability_error": check_error,
            "unreachable_servers": if check_error.is_none() {
                serde_json::json!(unreachable)
            } else {
                serde_json::Value::Null
            },
        }))?;
    }
    Ok(())
}

/// Long enough for a first handshake across continents, short enough to wait on.
const HANDSHAKE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

pub fn disconnect(value: Option<&str>) -> Result<()> {
    // Read connected networks under the same lock that connect switches with.
    let _switch = if output::is_dry_run() {
        None
    } else {
        Some(switch_lock()?)
    };
    let value = value.or(network::network_flag());
    let targets = match value {
        Some(_) => {
            let alias = network::resolve_network(value)?;
            let profile = local_profile(&alias)?;
            vec![(alias, profile)]
        }
        None => connected_networks()?,
    };
    let names: Vec<_> = targets
        .iter()
        .map(|(alias, _)| alias.alias.clone())
        .collect();
    if output::is_dry_run() {
        return finish(
            "Would disconnect from the network",
            serde_json::json!({"dry_run": true, "networks": names}),
        );
    }
    if targets.is_empty() {
        return finish(
            "Not connected to a network",
            serde_json::json!({"networks": names, "connected": false}),
        );
    }
    for (alias, profile) in &targets {
        teardown(alias, profile)?;
        output::success(&format!("Disconnected from {}", alias.alias));
    }
    if output::is_json() {
        output::json_result(serde_json::json!({"networks": names, "connected": false}))?;
    }
    Ok(())
}

fn teardown(alias: &crate::config::LocalNetwork, profile: &Profile) -> Result<()> {
    let _lifecycle = network::wireguard::lifecycle_lock(alias)?;
    network::wireguard::disconnect(alias, profile)?;
    network::local_access::remove(alias)
}

fn restore_all(networks: &[(crate::config::LocalNetwork, Profile)]) {
    for (alias, profile) in networks {
        match restore(alias, profile) {
            Ok(()) => output::muted(&format!("Reconnected to {}", alias.alias)),
            Err(error) => {
                output::warning(&format!("Could not reconnect to {}: {error}", alias.alias))
            }
        }
    }
}

fn restore(alias: &crate::config::LocalNetwork, profile: &Profile) -> Result<()> {
    let _lifecycle = network::wireguard::lifecycle_lock(alias)?;
    network::wireguard::connect(alias, profile)?;
    network::local_access::install(alias, profile)
}

/// Serializes network switches on this machine; one network owns `.tako`.
fn switch_lock() -> Result<std::fs::File> {
    let directory = crate::paths::network_state_dir()?.join("networks");
    network::private_directory(&directory)?;
    let lock = network::private_file(&directory.join("connect.lock"), true)?;
    lock.lock()?;
    Ok(lock)
}

/// Networks with an owned local connection configuration.
fn connected_networks() -> Result<Vec<(crate::config::LocalNetwork, Profile)>> {
    let mut connected = Vec::new();
    for alias in LocalNetworks::load()?.networks.into_values() {
        let profile = match local_profile(&alias) {
            Ok(profile) => profile,
            // A live connection must not be mistaken for none: switching would
            // leave two networks claiming `.tako`.
            Err(error) if network::wireguard::has_saved_connection(&alias)? => {
                return Err(format!(
                    "Network {} is connected, but its profile cannot be verified: {error}",
                    alias.alias
                )
                .into());
            }
            Err(_) => continue,
        };
        if network::wireguard::is_connected(&alias, &profile)? {
            connected.push((alias, profile));
        }
    }
    Ok(connected)
}

/// Local teardown must remain possible after membership expires or is revoked.
fn local_profile(alias: &crate::config::LocalNetwork) -> Result<Profile> {
    let profile: Profile = network::read_json(&alias.profile_path()?)?;
    if profile.snapshot().network.id.to_string() != alias.id
        || profile
            .request()
            .device_id_for(&profile.snapshot().network.id)?
            .to_string()
            != alias.device
    {
        return Err("Saved network alias does not match its profile".into());
    }
    profile.snapshot().validate()?;
    profile.request().verify()?;
    Ok(profile)
}

pub fn ssh(server: &str, value: Option<&str>, remote_command: &[String]) -> Result<()> {
    let alias = network::resolve_network(value)?;
    let profile = if output::is_dry_run() {
        network::load_profile(&alias)?
    } else {
        network::client::refresh(&alias)?
    };
    let servers = network::server_members(&profile)?;
    let matches: Vec<_> = servers
        .iter()
        .filter(|(id, member)| id.to_string() == server || member.request.name.as_str() == server)
        .collect();
    let [(id, member)] = matches.as_slice() else {
        return Err(format!(
            "Server '{server}' must uniquely match a name or device ID in this network"
        )
        .into());
    };
    let MemberKind::Server(info) = &member.kind else {
        unreachable!()
    };
    let own = profile
        .request()
        .device_id_for(&profile.snapshot().network.id)?;
    let permissions = profile.snapshot().permissions(&own, network::now()?)?;
    let user = permissions
        .ssh_user()
        .ok_or("Deploy clients have no SSH access")?;
    if output::is_dry_run() {
        return finish(
            "Would connect using the approved SSH identity",
            serde_json::json!({"dry_run": true, "network": alias.alias, "server": id.to_string(), "user": user.as_str()}),
        );
    }
    let keys = LocalDeviceKeys::load(&network::keys_path(&profile.request().request_id)?)?;
    let mut identity = tempfile::NamedTempFile::new()?;
    identity.write_all(keys.ssh_private_key()?.as_bytes())?;
    let mut known_hosts = tempfile::NamedTempFile::new()?;
    let host_alias = format!("tako-{id}");
    writeln!(known_hosts, "{host_alias} {}", info.ssh_host_key)?;
    let address = profile
        .snapshot()
        .network
        .id
        .address(&member.request.wg_public_key);
    let proxy =
        network::ssh_tunnel::proxy_command(&std::env::current_exe()?, &alias.id, &id.to_string())?;
    let mut command = std::process::Command::new("ssh");
    command
        .args([
            "-F",
            "/dev/null",
            "-a",
            "-x",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "IdentityAgent=none",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "UpdateHostKeys=no",
        ])
        .arg("-o")
        .arg(format!("HostKeyAlias={host_alias}"))
        .arg("-o")
        .arg(format!("ProxyCommand={proxy}"))
        .arg("-o")
        .arg("ConnectTimeout=15")
        .arg("-o")
        .arg(format!(
            "UserKnownHostsFile={}",
            network::local_access::quote_path(known_hosts.path())?
        ))
        .arg("-o")
        .arg(format!(
            "IdentityFile={}",
            network::local_access::quote_path(identity.path())?
        ))
        .arg("-p")
        .arg(info.ssh_port.to_string());
    let status = network::ssh_session::run(
        &alias,
        &id.to_string(),
        &command,
        &format!("{}@{address}", user.as_str()),
        &network::ssh_session::settings(
            user.as_str(),
            info.ssh_port,
            &info.ssh_host_key.to_string(),
            &address.to_string(),
        ),
        remote_command,
    )?;
    if !status.success() {
        return Err(format!("SSH exited with {status}").into());
    }
    Ok(())
}

pub(crate) fn validate_pending_approval(
    profile: &Profile,
    issuer: &tako_network::DeviceId,
    grant: &tako_network::api::ApproveRequest,
    pending: &tako_network::PendingApprovalProfile,
) -> Result<()> {
    if pending.request != grant.request
        || pending.pending_id != grant.pending_id
        || pending.issuer != *issuer
        || pending.expires_at != grant.expires_at
        || pending.snapshot.network != profile.snapshot().network
    {
        return Err("Approval response does not match the requested enrollment".into());
    }
    let servers: std::collections::BTreeMap<_, _> = pending
        .snapshot
        .effective_members(network::now()?)?
        .into_iter()
        .filter_map(|(id, member)| match &member.kind {
            MemberKind::Server(info) => Some((id, info.clone())),
            MemberKind::Laptop => None,
        })
        .collect();
    let known = network::server_members(profile)?;
    let expected = known
        .get(issuer)
        .ok_or("Approval issuer is not an active server")?;
    let MemberKind::Server(info) = &expected.kind else {
        unreachable!()
    };
    if pending.trusted_servers != servers || pending.trusted_servers.get(issuer) != Some(info) {
        return Err("Approval response changed the issuing server's identity".into());
    }
    Ok(())
}

fn status(value: Option<&str>) -> Result<()> {
    let alias = network::resolve_network(value)?;
    let profile = if output::is_dry_run() {
        network::load_profile(&alias)?
    } else {
        network::client::refresh(&alias)?
    };
    let members = profile.snapshot().effective_members(network::now()?)?;
    if output::is_json() {
        return output::json_result(
            serde_json::json!({"network": alias, "members": members, "revocations": profile.snapshot().revocations}),
        );
    }
    output::success(&format!("Network {}", network::network_label(&alias)));
    for (id, member) in members {
        let role = if matches!(member.kind, MemberKind::Server(_)) {
            "server"
        } else {
            "device"
        };
        let line = format!("{}  {role}  {id}", member.request.name);
        output::info(&line);
        tracing::info!("{line}");
    }
    Ok(())
}
