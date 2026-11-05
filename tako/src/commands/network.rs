use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use tako_network::{
    Approval, DeviceKind, DeviceName, LocalDeviceKeys, MemberKind, NetworkDescriptor, NetworkId,
    Permissions, RegistrySnapshot, WgChallenge,
};

use crate::config::NetworksToml;
use crate::network::{self, Profile, Result};
use crate::output;

#[derive(Subcommand)]
pub enum NetworkCommands {
    /// Create a private network and this machine's administrator identity
    Create { name: String },
    /// Generate this machine's keys and write a public enrollment request
    Request {
        #[arg(long)]
        name: Option<String>,
        /// Optional network ID or a locally configured alias
        #[arg(long)]
        network: Option<String>,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Approve a machine's request with explicit permissions
    Approve {
        file: PathBuf,
        #[arg(long)]
        network: Option<String>,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
        /// Grant network and server administration
        #[arg(long)]
        admin: bool,
        /// Grant a Tako administrator shell (requires --admin)
        #[arg(long, requires = "admin")]
        ssh: bool,
        /// Grant deployment scope as app/environment; repeat for additional scopes
        #[arg(long, value_name = "APP/ENV")]
        deploy: Vec<tako_network::AppEnv>,
    },
    /// Revoke a device and approvals derived from it
    Revoke {
        device: String,
        #[arg(long)]
        network: Option<String>,
    },
    /// Refresh and inspect network membership
    Status { network: Option<String> },
    /// Import an approved profile for keys generated on this machine
    Import {
        file: PathBuf,
        /// Local alias for the imported network
        #[arg(long)]
        name: Option<String>,
    },
    /// Export public network configuration without private keys or app secrets
    Export {
        network: Option<String>,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
    /// List this machine's network aliases and stable IDs
    #[command(visible_alias = "ls")]
    List,
}

pub fn run(command: NetworkCommands, config_path: Option<&Path>) -> Result<()> {
    match command {
        NetworkCommands::Create { name } => create(&name),
        NetworkCommands::Request {
            name,
            network,
            output,
        } => request(name.as_deref(), network.as_deref(), &output),
        NetworkCommands::Import { file, name } => import(&file, name.as_deref()),
        NetworkCommands::Export { network, output } => {
            export(network.as_deref(), config_path, &output)
        }
        NetworkCommands::List => list(),
        NetworkCommands::Approve {
            file,
            network,
            output,
            admin,
            ssh,
            deploy,
        } => approve(
            &file,
            network.as_deref(),
            config_path,
            &output,
            Permissions {
                network_admin: admin,
                server_admin: admin,
                ssh_user: ssh.then(|| "tako".parse()).transpose()?,
                deployments: deploy.into_iter().collect(),
            },
        ),
        NetworkCommands::Revoke { device, network } => {
            revoke(&device, network.as_deref(), config_path)
        }
        NetworkCommands::Status { network } => status(network.as_deref(), config_path),
    }
}

fn create(name: &str) -> Result<()> {
    crate::config::validate_network_name(name)?;
    let aliases = NetworksToml::load()?;
    if aliases.networks.contains_key(name) {
        return Err(format!("Network alias '{name}' already exists").into());
    }
    if output::is_dry_run() {
        return finish(
            "Would create a network and local device keys",
            serde_json::json!({"dry_run": true, "name": name}),
        );
    }
    let id = NetworkId::random()?;
    let keys = LocalDeviceKeys::generate()?;
    let request =
        keys.enrollment_request(Some(id.clone()), DeviceKind::Laptop, device_name(None)?)?;
    let now = network::now()?;
    let (challenge, pending) = WgChallenge::begin(&request, id.clone(), now, now + 60)?;
    let verified = pending.verify(&keys.answer_challenge(&challenge, &request, now)?, now)?;
    let mut snapshot = RegistrySnapshot {
        network: NetworkDescriptor {
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
            permissions: Permissions {
                network_admin: true,
                server_admin: true,
                ssh_user: Some("tako".parse()?),
                deployments: BTreeSet::new(),
            },
            approved_by: None,
            independent_admin: false,
            created_at: now,
            expires_at: None,
        },
    )?);
    network::store_keys(&request.request_id, &keys)?;
    let alias = network::save_profile(name, &Profile::Created { request, snapshot })?;
    finish(
        &format!("Created network {name}"),
        serde_json::to_value(&alias)?,
    )?;
    output::hint(&format!(
        "Add a server with `tako servers add <admin@host> --network {name}`."
    ));
    Ok(())
}

fn request(name: Option<&str>, network: Option<&str>, destination: &Path) -> Result<()> {
    let name = device_name(name)?;
    let network = network
        .map(|value| -> Result<NetworkId> {
            if let Ok(id) = value.parse::<NetworkId>() {
                return Ok(id);
            }
            Ok(NetworksToml::load()?.select(Some(value))?.id.parse()?)
        })
        .transpose()?;
    if output::is_dry_run() {
        return finish(
            "Would generate local device keys and an enrollment request",
            serde_json::json!({"dry_run": true, "output": destination}),
        );
    }
    let keys = LocalDeviceKeys::generate()?;
    let request = keys.enrollment_request_expiring(
        network,
        DeviceKind::Laptop,
        name,
        Some(network::now()? + 24 * 60 * 60),
    )?;
    network::store_keys(&request.request_id, &keys)?;
    if let Err(error) = network::write_json(destination, &request, false) {
        let _ = std::fs::remove_file(network::keys_path(&request.request_id)?);
        return Err(error);
    }
    finish(
        "Wrote enrollment request",
        serde_json::json!({"output": destination, "request_id": request.request_id, "name": request.name}),
    )?;
    output::hint("Transfer this request to an administrator. Private keys remain on this machine.");
    Ok(())
}

fn import(path: &Path, name: Option<&str>) -> Result<()> {
    let profile: Profile = network::read_json(path)?;
    let id = profile.snapshot().network.id.to_string();
    let aliases = NetworksToml::load()?;
    let default_name = format!("network-{}", &id[..8]);
    let name = name
        .or_else(|| {
            aliases
                .networks
                .values()
                .find(|alias| alias.id == id)
                .map(|alias| alias.name.as_str())
        })
        .unwrap_or(&default_name);
    let keys = LocalDeviceKeys::load(&network::keys_path(&profile.request().request_id)?)
        .map_err(|_| "This profile needs the keys from its enrollment request. Run `tako network request` on this machine and have an administrator approve it.")?;
    profile.validate(&keys)?;
    if output::is_dry_run() {
        return finish(
            "Would import the network profile",
            serde_json::json!({"dry_run": true, "name": name, "id": id}),
        );
    }
    let alias = network::save_profile(name, &profile)?;
    finish(
        &format!("Imported network {name}"),
        serde_json::to_value(alias)?,
    )?;
    output::hint(&format!("Connect with `tako connect {name}`."));
    Ok(())
}

fn export(value: Option<&str>, config_path: Option<&Path>, destination: &Path) -> Result<()> {
    let alias = network::resolve_network(value, config_path)?;
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
    let networks = NetworksToml::load()?;
    if output::is_json() {
        return output::json_result(
            serde_json::json!({"networks": networks.networks.values().collect::<Vec<_>>()}),
        );
    }
    for network in networks.networks.values() {
        output::info(&format!("{}  {}", network.name, network.id));
        tracing::info!("{}  {}", network.name, network.id);
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

pub fn connect(value: Option<&str>, config_path: Option<&Path>) -> Result<()> {
    let alias = network::resolve_network(value, config_path)?;
    if output::is_dry_run() {
        network::load_profile(&alias)?;
        return finish(
            "Would connect to the network",
            serde_json::json!({"dry_run": true, "network": alias.name}),
        );
    }
    let profile = network::client::refresh(&alias)?;
    network::wireguard::connect(&alias, &profile)?;
    finish(
        &format!("Connected to {}", alias.name),
        serde_json::json!({"network": alias.name, "connected": true}),
    )
}

pub fn disconnect(value: Option<&str>, config_path: Option<&Path>) -> Result<()> {
    let alias = network::resolve_network(value, config_path)?;
    // Local teardown must remain possible after membership expires or is revoked.
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
    if output::is_dry_run() {
        return finish(
            "Would disconnect from the network",
            serde_json::json!({"dry_run": true, "network": alias.name}),
        );
    }
    network::wireguard::disconnect(&alias, &profile)?;
    finish(
        &format!("Disconnected from {}", alias.name),
        serde_json::json!({"network": alias.name, "connected": false}),
    )
}

pub fn ssh(
    server: &str,
    value: Option<&str>,
    config_path: Option<&Path>,
    remote_command: &[String],
) -> Result<()> {
    let alias = network::resolve_network(value, config_path)?;
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
        .ssh_user
        .as_ref()
        .ok_or("This device has no SSH permission")?;
    if output::is_dry_run() {
        return finish(
            "Would connect using the approved SSH identity",
            serde_json::json!({"dry_run": true, "network": alias.name, "server": id.to_string(), "user": user.as_str()}),
        );
    }
    network::wireguard::connect(&alias, &profile)?;
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
    let status = std::process::Command::new("ssh")
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
        .arg(format!(
            "UserKnownHostsFile={}",
            known_hosts.path().display()
        ))
        .arg("-i")
        .arg(identity.path())
        .arg("-p")
        .arg(info.ssh_port.to_string())
        .arg(format!("{}@{address}", user.as_str()))
        .args(remote_command)
        .status()?;
    if !status.success() {
        return Err(format!("SSH exited with {status}").into());
    }
    Ok(())
}

fn approve(
    file: &Path,
    value: Option<&str>,
    config_path: Option<&Path>,
    destination: &Path,
    permissions: Permissions,
) -> Result<()> {
    use tako_network::api::{ApproveRequest, RpcRequest, RpcResponse};
    let request: tako_network::EnrollmentRequest = network::read_json(file)?;
    request.verify()?;
    permissions.validate()?;
    if request.kind != DeviceKind::Laptop {
        return Err("Use `tako servers add --network` to enroll a server".into());
    }
    let alias = network::resolve_network(value, config_path)?;
    request.device_id_for(&alias.id.parse()?)?;
    if output::is_dry_run() {
        return finish(
            "Would approve the enrollment request",
            serde_json::json!({"dry_run": true, "network": alias.name, "device": request.name, "permissions": permissions}),
        );
    }
    if destination.try_exists()? {
        return Err("Approval output already exists; choose a new file".into());
    }
    let profile = network::client::refresh(&alias)?;
    let now = network::now()?;
    let expires_at = request.expires_at.unwrap_or(now + 86400).min(now + 86400);
    if expires_at <= now {
        return Err("Enrollment request has expired".into());
    }
    let grant = ApproveRequest {
        pending_id: tako_network::RequestId::random()?,
        request,
        kind: MemberKind::Laptop,
        permissions,
        expires_at,
        member_expires_at: None,
    };
    let issuer = network::client::server(&profile)?;
    let response = network::client::call(
        &profile,
        &issuer,
        RpcRequest::Approve(Box::new(grant.clone())),
    )?;
    let RpcResponse::Pending(approved) = response else {
        return Err("Unexpected approval response".into());
    };
    validate_pending_approval(&profile, &issuer, &grant, &approved)?;
    let device = approved.request.name.to_string();
    network::write_json(destination, &Profile::Pending { profile: approved }, false)?;
    finish(
        "Wrote approved network profile",
        serde_json::json!({"network": alias.name, "device": device, "output": destination}),
    )?;
    output::hint(
        "Transfer the profile through a trusted channel. On the new machine, run `tako network import <file>` and `tako connect`.",
    );
    Ok(())
}

fn revoke(device: &str, value: Option<&str>, config_path: Option<&Path>) -> Result<()> {
    use tako_network::api::{RpcRequest, RpcResponse};
    let alias = network::resolve_network(value, config_path)?;
    let profile = if output::is_dry_run() {
        network::load_profile(&alias)?
    } else {
        network::client::refresh(&alias)?
    };
    let members = profile.snapshot().effective_members(network::now()?)?;
    let candidates: Vec<_> = members
        .into_iter()
        .filter(|(id, member)| id.to_string() == device || member.request.name.as_str() == device)
        .collect();
    let [(target, member)] = candidates.as_slice() else {
        return Err("Device must uniquely match a name or device ID".into());
    };
    if output::is_dry_run() {
        return finish(
            "Would revoke the device and its delegated approvals",
            serde_json::json!({"dry_run": true, "network": alias.name, "device": target}),
        );
    }
    let id = tako_network::RevocationId::random()?;
    let command = RpcRequest::Revoke {
        id: id.clone(),
        target: target.clone(),
    };
    let RpcResponse::Revoked(revocation) =
        network::client::call(&profile, &network::client::server(&profile)?, command)?
    else {
        return Err("Unexpected revocation response".into());
    };
    if revocation.id != id
        || revocation.target != *target
        || revocation.by
            != profile
                .request()
                .device_id_for(&profile.snapshot().network.id)?
        || revocation.ssh_fingerprint != member.request.ssh_public_key.fingerprint()?
        || revocation.wg_fingerprint != member.request.wg_public_key.fingerprint()
    {
        return Err("Revocation response does not match the requested device".into());
    }
    network::record_revocation(&alias, revocation.clone())?;
    finish(
        "Recorded device revocation",
        serde_json::json!({"network": alias.name, "revocation": revocation, "enforcement": "pending"}),
    )?;
    output::hint(
        "Servers enforce the revocation as it reaches them. Unreachable servers remain pending.",
    );
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

fn status(value: Option<&str>, config_path: Option<&Path>) -> Result<()> {
    let alias = network::resolve_network(value, config_path)?;
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
    output::success(&format!("Network {} ({})", alias.name, alias.id));
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
