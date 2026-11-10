use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tako_network::{DeviceId, LocalDeviceKeys, MemberKind, NetworkId};

use super::{Profile, Result};
use crate::config::NetworkAlias;
use crate::output;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod backend;
use backend::Operation;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use backend::{HELPER_COMMAND, helper_main};

pub(crate) fn lifecycle_lock(alias: &NetworkAlias) -> Result<fs::File> {
    let lock = super::ssh_session::lock(alias, "lifecycle.lock")?;
    lock.lock()?;
    Ok(lock)
}

pub(crate) fn connect(alias: &NetworkAlias, profile: &Profile) -> Result<()> {
    connect_with(
        alias,
        profile,
        || authorize("set up the local network interface, private DNS, and server names"),
        backend::invoke,
    )
}

fn connect_with(
    alias: &NetworkAlias,
    profile: &Profile,
    authorize: impl FnOnce() -> Result<()>,
    mut execute: impl FnMut(Operation) -> Result<Option<String>>,
) -> Result<()> {
    if !matches!(profile, Profile::Active { .. }) {
        return Err("This network needs an active server approval before connecting".into());
    }
    let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
    let configuration = configuration(profile, &keys)?;
    let path = configuration_path(alias, profile)?;
    let parent = path.parent().unwrap();
    super::private_directory(parent)?;
    let lock = super::private_file(&parent.join("connection.lock"), true)?;
    lock.lock()?;
    let _ssh = super::ssh_session::prepare_connect(alias)?;
    let saved = saved_configuration(&path)?;
    let interface = interface_name(&profile.snapshot().network.id);
    let public_key = keys.wg_public_key()?.to_string();
    let network = profile.snapshot().network.id.to_string();
    let names = super::server_members(profile)?
        .values()
        .map(|member| backend::ServerName {
            name: member.request.name.to_string().to_ascii_lowercase(),
            address: profile
                .snapshot()
                .network
                .id
                .address(&member.request.wg_public_key),
        })
        .collect::<Vec<_>>();
    authorize()?;
    if let Some(active_key) = execute(Operation::Inspect {
        interface: interface.clone(),
    })? {
        if active_key != public_key {
            return Err("A different WireGuard connection already uses this interface".into());
        }
        let saved = saved
            .as_deref()
            .ok_or("Active WireGuard interface has no owned configuration")?;
        validate_saved_configuration(saved, &keys)?;
        if saved == configuration {
            execute(Operation::Sync {
                interface,
                configuration,
                network,
                names,
            })?;
            return Ok(());
        }
        execute(Operation::Stop {
            interface: interface.clone(),
            public_key,
        })?;
    }
    super::write_bytes(&path, configuration.as_bytes(), true)?;
    execute(Operation::Start {
        interface,
        configuration,
        network,
        names,
    })?;
    Ok(())
}

pub(crate) fn disconnect(alias: &NetworkAlias, profile: &Profile) -> Result<()> {
    disconnect_with(
        alias,
        profile,
        || authorize("remove the local network interface, private DNS, and server names"),
        backend::invoke,
    )
}

fn disconnect_with(
    alias: &NetworkAlias,
    profile: &Profile,
    authorize: impl FnOnce() -> Result<()>,
    mut execute: impl FnMut(Operation) -> Result<Option<String>>,
) -> Result<()> {
    let path = configuration_path(alias, profile)?;
    if saved_configuration(&path)?.is_none() {
        return Ok(());
    }
    let lock = super::private_file(&path.parent().unwrap().join("connection.lock"), true)?;
    lock.lock()?;
    let Some(saved) = saved_configuration(&path)? else {
        return Ok(());
    };
    let interface = interface_name(&profile.snapshot().network.id);
    authorize()?;
    if let Some(active_key) = execute(Operation::Inspect {
        interface: interface.clone(),
    })? {
        let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
        let public_key = keys.wg_public_key()?.to_string();
        if active_key != public_key {
            return Err("Refusing to disconnect a different WireGuard connection".into());
        }
        validate_saved_configuration(&saved, &keys)?;
        super::ssh_session::stop_masters(alias)?;
        execute(Operation::Stop {
            interface,
            public_key,
        })?;
    }
    fs::remove_file(path)?;
    Ok(())
}

fn saved_configuration(path: &Path) -> Result<Option<String>> {
    match super::private_file(path, false) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 4 * 1024 * 1024 {
                return Err("WireGuard configuration exceeds 4 MiB".into());
            }
            Ok(Some(String::from_utf8(bytes)?))
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn validate_saved_configuration(configuration: &str, keys: &LocalDeviceKeys) -> Result<()> {
    let mut lines = configuration.lines();
    if lines.next() != Some("[Interface]")
        || lines.next()
            != Some(format!("PrivateKey = {}", keys.wireguard_private_key()?.as_str()).as_str())
    {
        return Err("WireGuard configuration belongs to another device".into());
    }
    let address = lines
        .next()
        .and_then(|line| line.strip_prefix("Address = "))
        .ok_or("Missing WireGuard address")?;
    parse_route(address)?;
    while let Some(line) = lines.next() {
        if !line.is_empty() || lines.next() != Some("[Peer]") {
            return Err("Unexpected WireGuard configuration directive".into());
        }
        let key = lines
            .next()
            .and_then(|line| line.strip_prefix("PublicKey = "))
            .ok_or("Missing WireGuard peer key")?;
        let _: tako_network::WgPublicKey = key.parse()?;
        let route = lines
            .next()
            .and_then(|line| line.strip_prefix("AllowedIPs = "))
            .ok_or("Missing WireGuard peer route")?;
        parse_route(route)?;
        let endpoint = lines
            .next()
            .and_then(|line| line.strip_prefix("Endpoint = "))
            .ok_or("Missing WireGuard peer endpoint")?;
        let _: std::net::SocketAddr = endpoint.parse()?;
        if lines.next() != Some("PersistentKeepalive = 25") {
            return Err("Unexpected WireGuard keepalive".into());
        }
    }
    Ok(())
}

fn parse_route(route: &str) -> Result<()> {
    let address = route
        .strip_suffix("/128")
        .ok_or("WireGuard routes must be single hosts")?;
    let _: std::net::Ipv6Addr = address.parse()?;
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

fn configuration_path(alias: &NetworkAlias, profile: &Profile) -> Result<PathBuf> {
    let id: NetworkId = alias.id.parse()?;
    let device: DeviceId = alias.device.parse()?;
    if id != profile.snapshot().network.id || device != profile.request().device_id_for(&id)? {
        return Err("WireGuard alias does not match its profile".into());
    }
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

fn authorize(action: &str) -> Result<()> {
    if output::is_root() {
        return Ok(());
    }
    let status = Command::new("sudo")
        .args(["-n", "-v"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("Could not run sudo: {error}"))?;
    if status.success() {
        return Ok(());
    }
    if !output::is_interactive() {
        return Err(format!(
            "Administrator access is required to {action}. Run this command in a terminal to authorize with sudo."
        )
        .into());
    }
    output::hint(&format!("Tako needs administrator access to {action}."));
    // Keep authentication on the terminal before capturing tool diagnostics.
    let status = Command::new("sudo")
        .args(["-v", "-p", "Local password for %p: "])
        .status()
        .map_err(|error| format!("Could not run sudo: {error}"))?;
    if !status.success() {
        return Err("Administrator authorization failed".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::{
        self,
        tests::{TestHome, fixture},
    };
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn configuration_routes_only_to_server_hosts_and_uses_own_private_key() {
        let fixture = fixture();
        let config = configuration(&fixture.profile, &fixture.keys).unwrap();
        validate_saved_configuration(&config, &fixture.keys).unwrap();
        let network = &fixture.profile.snapshot().network.id;
        let own = network.address(&fixture.keys.wg_public_key().unwrap());
        let server = fixture
            .profile
            .snapshot()
            .members
            .iter()
            .find(|member| member.device_id == fixture.server)
            .unwrap();
        let address = network.address(&server.request.wg_public_key);
        assert!(config.contains(&format!(
            "PrivateKey = {}\n",
            fixture.keys.wireguard_private_key().unwrap().as_str()
        )));
        assert!(config.contains(&format!("Address = {own}/128\n")));
        // Inspect routing fields, not the whole configuration: random base64
        // keys can contain "/0" without installing a default route.
        let allowed_ips: Vec<_> = config
            .lines()
            .filter_map(|line| line.strip_prefix("AllowedIPs = "))
            .collect();
        assert_eq!(allowed_ips, vec![format!("{address}/128")]);
        assert_eq!(config.matches("[Peer]").count(), 1);
        assert!(config.contains("Endpoint = 192.0.2.10:51820\n"));
        assert!(config.contains("PersistentKeepalive = 25\n"));
        assert!(configuration(&fixture.profile, &LocalDeviceKeys::generate().unwrap()).is_err());
        assert!(interface_name(network).len() <= 15);
    }

    #[test]
    fn saved_configuration_cannot_execute_hooks_or_claim_another_key() {
        let fixture = fixture();
        let config = configuration(&fixture.profile, &fixture.keys).unwrap();
        assert!(
            validate_saved_configuration(
                &format!("{config}PostDown = touch /tmp/unsafe\n"),
                &fixture.keys
            )
            .is_err()
        );
        assert!(
            validate_saved_configuration(&config.replace("/128", "/0"), &fixture.keys).is_err()
        );
        assert!(
            validate_saved_configuration(&config, &LocalDeviceKeys::generate().unwrap()).is_err()
        );
    }

    #[test]
    fn saved_configuration_and_lock_reject_links_and_unsafe_modes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tako.conf");
        network::write_bytes(&path, b"private", false).unwrap();
        let original = fs::read(&path).unwrap();
        for hard in [false, true] {
            let link = directory.path().join("linked");
            if hard {
                fs::hard_link(&path, &link).unwrap();
            } else {
                symlink(&path, &link).unwrap();
            }
            assert!(saved_configuration(&link).is_err());
            assert!(network::private_file(&link, true).is_err());
            assert_eq!(fs::read(&path).unwrap(), original);
            fs::remove_file(link).unwrap();
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(saved_configuration(&path).is_err());
        assert!(network::private_file(&path, true).is_err());
    }

    #[test]
    fn connect_reconciles_unchanged_peers_and_disconnect_checks_ownership() {
        let home = TestHome::new();
        let fixture = fixture();
        network::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
        let alias = network::save_profile("test", &fixture.profile).unwrap();
        let mut events = Vec::new();
        connect_with(
            &alias,
            &fixture.profile,
            || Ok(()),
            |op| {
                match op {
                    Operation::Inspect { .. } => events.push("inspect"),
                    Operation::Start {
                        ref configuration, ..
                    } => {
                        validate_saved_configuration(configuration, &fixture.keys)?;
                        events.push("start");
                    }
                    _ => panic!("unexpected operation"),
                }
                Ok(None)
            },
        )
        .unwrap();
        assert_eq!(events, ["inspect", "start"]);
        let path = configuration_path(&alias, &fixture.profile).unwrap();
        assert!(path.starts_with(home.directory.path()));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let saved = fs::read(&path).unwrap();
        let key = fixture.keys.wg_public_key().unwrap().to_string();
        events.clear();
        connect_with(
            &alias,
            &fixture.profile,
            || Ok(()),
            |op| {
                match op {
                    Operation::Inspect { .. } => return Ok(Some(key.clone())),
                    Operation::Sync {
                        ref configuration, ..
                    } => {
                        assert_eq!(configuration.as_bytes(), saved);
                        events.push("sync");
                    }
                    _ => panic!("reconnect must retain the interface and routes"),
                }
                Ok(None)
            },
        )
        .unwrap();
        assert_eq!(events, ["sync"]);
        for disconnect in [false, true] {
            let execute = |op| match op {
                Operation::Inspect { .. } => Ok(Some("another-device".into())),
                _ => panic!("must not change another device's interface"),
            };
            let result = if disconnect {
                disconnect_with(&alias, &fixture.profile, || Ok(()), execute)
            } else {
                connect_with(&alias, &fixture.profile, || Ok(()), execute)
            };
            assert!(result.is_err());
            assert_eq!(fs::read(&path).unwrap(), saved);
        }
        disconnect_with(
            &alias,
            &fixture.profile,
            || Ok(()),
            |op| match op {
                Operation::Inspect { .. } => Ok(Some(key.clone())),
                Operation::Stop { ref public_key, .. } => {
                    assert_eq!(public_key, &key);
                    Ok(None)
                }
                _ => panic!("unexpected operation"),
            },
        )
        .unwrap();
        assert!(!path.exists());
        disconnect_with(
            &alias,
            &fixture.profile,
            || panic!("nothing to disconnect"),
            |_| panic!("no interface operations without a configuration"),
        )
        .unwrap();
    }

    #[test]
    fn denied_authorization_and_failed_stop_preserve_saved_state() {
        let _home = TestHome::new();
        let fixture = fixture();
        network::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
        let alias = network::save_profile("test", &fixture.profile).unwrap();
        let path = configuration_path(&alias, &fixture.profile).unwrap();
        assert!(
            connect_with(
                &alias,
                &fixture.profile,
                || Err("authorization denied".into()),
                |_| panic!("must authorize before inspecting interfaces"),
            )
            .is_err()
        );
        assert!(!path.exists());
        connect_with(&alias, &fixture.profile, || Ok(()), |_| Ok(None)).unwrap();
        let saved = fs::read(&path).unwrap();
        assert!(
            disconnect_with(
                &alias,
                &fixture.profile,
                || Err("authorization denied".into()),
                |_| panic!("must authorize before inspecting interfaces"),
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), saved);
        let key = fixture.keys.wg_public_key().unwrap().to_string();
        let mut changed = fixture.profile.clone();
        let Profile::Active { profile } = &mut changed else {
            panic!()
        };
        let server = profile
            .snapshot
            .members
            .iter_mut()
            .find(|member| member.device_id == fixture.server)
            .unwrap();
        let MemberKind::Server(info) = &mut server.kind else {
            panic!()
        };
        info.wireguard_endpoint = "192.0.2.11:51820".parse().unwrap();
        profile
            .trusted_servers
            .insert(fixture.server.clone(), info.clone());
        assert!(
            connect_with(
                &alias,
                &changed,
                || Ok(()),
                |op| match op {
                    Operation::Inspect { .. } => Ok(Some(key.clone())),
                    Operation::Stop { .. } => Err("stop failed".into()),
                    _ => panic!("failed stop must not replace the interface"),
                }
            )
            .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), saved);
    }
}
