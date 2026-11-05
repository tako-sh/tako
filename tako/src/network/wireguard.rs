use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tako_network::{DeviceId, LocalDeviceKeys, MemberKind, NetworkId};

use super::{Profile, Result};
use crate::config::NetworkAlias;

pub(crate) fn connect(alias: &NetworkAlias, profile: &Profile) -> Result<()> {
    if !matches!(profile, Profile::Active { .. }) {
        return Err("This network needs an active server approval before connecting".into());
    }
    let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
    let configuration = configuration(profile, &keys)?;
    let path = configuration_path(alias)?;
    let parent = path.parent().unwrap();
    super::private_directory(parent)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(parent.join("connection.lock"))?;
    lock.lock()?;
    let interface = interface_name(&profile.snapshot().network.id);
    if let Some(active) = active_interface(&interface)? {
        let public_key = capture("wg", &["show", &active, "public-key"])?;
        if public_key.trim() != keys.wg_public_key()?.to_string() {
            return Err("A different WireGuard connection already uses this interface".into());
        }
        if fs::read_to_string(&path).ok().as_deref() == Some(&configuration) {
            return Ok(());
        }
        run("wg-quick", &["down", path_string(&path)?])?;
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(configuration.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(&path)?;
    run("wg-quick", &["up", path_string(&path)?])
}

pub(crate) fn disconnect(alias: &NetworkAlias, profile: &Profile) -> Result<()> {
    let path = configuration_path(alias)?;
    if !path.exists() {
        return Ok(());
    }
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.parent().unwrap().join("connection.lock"))?;
    lock.lock()?;
    let interface = interface_name(&profile.snapshot().network.id);
    if let Some(active) = active_interface(&interface)? {
        let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
        if capture("wg", &["show", &active, "public-key"])?.trim()
            != keys.wg_public_key()?.to_string()
        {
            return Err("Refusing to disconnect a different WireGuard connection".into());
        }
        run("wg-quick", &["down", path_string(&path)?])?;
    }
    fs::remove_file(path)?;
    Ok(())
}

fn configuration(profile: &Profile, keys: &LocalDeviceKeys) -> Result<String> {
    profile.validate(keys)?;
    let network = &profile.snapshot().network.id;
    let own = profile.request().device_id_for(network)?;
    let members = profile.snapshot().effective_members(super::now()?)?;
    if !members.contains_key(&own) {
        return Err("This device is no longer a network member".into());
    }
    let address = network.address(&keys.wg_public_key()?);
    let mut value = format!(
        "[Interface]\nPrivateKey = {}\nAddress = {address}/128\n",
        keys.wireguard_private_key()?.as_str()
    );
    let mut servers = 0;
    for (id, member) in members {
        if id == own {
            continue;
        }
        if let MemberKind::Server(server) = &member.kind {
            let address = network.address(&member.request.wg_public_key);
            value.push_str(&format!("\n[Peer]\nPublicKey = {}\nAllowedIPs = {address}/128\nEndpoint = {}\nPersistentKeepalive = 25\n", member.request.wg_public_key, server.wireguard_endpoint));
            servers += 1;
        }
    }
    if servers == 0 {
        return Err("No active servers are available in this network".into());
    }
    Ok(value)
}

fn configuration_path(alias: &NetworkAlias) -> Result<PathBuf> {
    let id: NetworkId = alias.id.parse()?;
    let _: DeviceId = alias.device.parse()?;
    Ok(alias
        .profile_path()?
        .parent()
        .unwrap()
        .join("wireguard")
        .join(format!("{}.conf", interface_name(&id))))
}

fn interface_name(network: &NetworkId) -> String {
    format!("tako{}", &network.to_string()[..10])
}

fn active_interface(interface: &str) -> Result<Option<String>> {
    let interfaces = capture("wg", &["show", "interfaces"])?;
    #[cfg(target_os = "macos")]
    let actual = match fs::read_to_string(format!("/var/run/wireguard/{interface}.name")) {
        Ok(value) => value.trim().to_string(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    #[cfg(not(target_os = "macos"))]
    let actual = interface.to_string();
    Ok(interfaces
        .split_whitespace()
        .any(|name| name == actual)
        .then_some(actual))
}

fn path_string(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| "WireGuard configuration path must be UTF-8".into())
}

fn privileged(program: &str) -> Command {
    #[cfg(unix)]
    if unsafe { libc::geteuid() } == 0 {
        return Command::new(program);
    }
    let mut command = Command::new("sudo");
    command.arg("--").arg(program);
    command
}

fn capture(program: &str, args: &[&str]) -> Result<String> {
    let result = privileged(program)
        .args(args)
        .stderr(Stdio::inherit())
        .output()?;
    if !result.status.success() {
        return Err(format!("{program} failed. Install the official WireGuard tools and allow local administrator privileges.").into());
    }
    Ok(String::from_utf8(result.stdout)?)
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = privileged(program)
        .args(args)
        .stdout(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(format!("{program} failed; the network is not connected").into());
    }
    Ok(())
}
