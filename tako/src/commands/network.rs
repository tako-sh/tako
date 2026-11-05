use std::collections::BTreeSet;
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
