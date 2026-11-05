use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tako_network::{DeviceId, LocalDeviceKeys, MemberKind, NetworkId};

use super::{Profile, Result};
use crate::config::NetworkAlias;

pub(crate) fn connect(alias: &NetworkAlias, profile: &Profile) -> Result<()> {
    connect_with(alias, profile, capture, run)
}

fn connect_with(
    alias: &NetworkAlias,
    profile: &Profile,
    mut capture: impl FnMut(&str, &[&str]) -> Result<String>,
    mut run: impl FnMut(&str, &[&str]) -> Result<()>,
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
    let saved = saved_configuration(&path)?;
    let interface = interface_name(&profile.snapshot().network.id);
    if let Some(active) = active_interface(&interface, &mut capture)? {
        let public_key = capture("wg", &["show", &active, "public-key"])?;
        if public_key.trim() != keys.wg_public_key()?.to_string() {
            return Err("A different WireGuard connection already uses this interface".into());
        }
        let saved = saved
            .as_deref()
            .ok_or("Active WireGuard interface has no owned configuration")?;
        validate_saved_configuration(saved, &keys)?;
        run("wg-quick", &["down", path_string(&path)?])?;
    }
    super::write_bytes(&path, configuration.as_bytes(), true)?;
    run("wg-quick", &["up", path_string(&path)?])
}

pub(crate) fn disconnect(alias: &NetworkAlias, profile: &Profile) -> Result<()> {
    disconnect_with(alias, profile, capture, run)
}

fn disconnect_with(
    alias: &NetworkAlias,
    profile: &Profile,
    mut capture: impl FnMut(&str, &[&str]) -> Result<String>,
    mut run: impl FnMut(&str, &[&str]) -> Result<()>,
) -> Result<()> {
    let path = configuration_path(alias, profile)?;
    let Some(_) = saved_configuration(&path)? else {
        return Ok(());
    };
    let lock = super::private_file(&path.parent().unwrap().join("connection.lock"), true)?;
    lock.lock()?;
    let Some(saved) = saved_configuration(&path)? else {
        return Ok(());
    };
    let interface = interface_name(&profile.snapshot().network.id);
    if let Some(active) = active_interface(&interface, &mut capture)? {
        let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
        if capture("wg", &["show", &active, "public-key"])?.trim()
            != keys.wg_public_key()?.to_string()
        {
            return Err("Refusing to disconnect a different WireGuard connection".into());
        }
        validate_saved_configuration(&saved, &keys)?;
        run("wg-quick", &["down", path_string(&path)?])?;
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

fn active_interface(
    interface: &str,
    capture: &mut impl FnMut(&str, &[&str]) -> Result<String>,
) -> Result<Option<String>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::{
        self,
        tests::{TestHome, fixture},
    };
    use std::cell::RefCell;
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
        assert!(config.contains(&format!("AllowedIPs = {address}/128\n")));
        assert_eq!(config.matches("[Peer]").count(), 1);
        assert_eq!(config.matches("AllowedIPs = ").count(), 1);
        assert!(!config.contains("/0"));
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
    #[cfg(target_os = "linux")]
    fn reconnect_reapplies_configuration_and_refuses_another_interfaces_key() {
        let home = TestHome::new();
        let fixture = fixture();
        network::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
        let alias = network::save_profile("test", &fixture.profile).unwrap();
        let interface = interface_name(&fixture.profile.snapshot().network.id);
        let key = fixture.keys.wg_public_key().unwrap().to_string();
        let events = RefCell::new(Vec::new());
        let mut run = |program: &str, args: &[&str]| -> Result<()> {
            assert_eq!(program, "wg-quick");
            let path = Path::new(args[1]);
            assert!(path.starts_with(home.directory.path()));
            assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
            validate_saved_configuration(&fs::read_to_string(path)?, &fixture.keys)?;
            events.borrow_mut().push(args[0].to_string());
            Ok(())
        };
        connect_with(&alias, &fixture.profile, |_, _| Ok(String::new()), &mut run).unwrap();
        assert_eq!(*events.borrow(), ["up"]);
        let capture = |_: &str, args: &[&str]| -> Result<String> {
            Ok(if args == ["show", "interfaces"] {
                interface.clone()
            } else {
                key.clone()
            })
        };
        connect_with(&alias, &fixture.profile, capture, &mut run).unwrap();
        assert_eq!(*events.borrow(), ["up", "down", "up"]);
        let path = configuration_path(&alias, &fixture.profile).unwrap();
        let saved = fs::read(&path).unwrap();
        assert!(
            connect_with(
                &alias,
                &fixture.profile,
                |_, args| {
                    Ok(if args == ["show", "interfaces"] {
                        interface.clone()
                    } else {
                        "someone-else".into()
                    })
                },
                &mut run
            )
            .is_err()
        );
        assert_eq!(*events.borrow(), ["up", "down", "up"]);
        assert_eq!(fs::read(&path).unwrap(), saved);
        let mut revoked = fixture.profile.clone();
        let Profile::Active { profile } = &mut revoked else {
            panic!()
        };
        let own = profile.snapshot.network.initial_admin.clone();
        profile.snapshot.revocations.push(
            profile
                .snapshot
                .plan_revocation(
                    tako_network::RevocationId::random().unwrap(),
                    &own,
                    &own,
                    network::now().unwrap(),
                )
                .unwrap(),
        );
        assert!(configuration(&revoked, &fixture.keys).is_err());
        disconnect_with(&alias, &revoked, capture, &mut run).unwrap();
        assert_eq!(*events.borrow(), ["up", "down", "up", "down"]);
        assert!(!path.exists());
        disconnect_with(
            &alias,
            &revoked,
            |_, _| panic!("no configuration means no WG calls"),
            |_, _| panic!("no configuration means no WG calls"),
        )
        .unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn failed_reconnect_preserves_configuration_and_propagates_failure() {
        let _home = TestHome::new();
        let fixture = fixture();
        network::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
        let alias = network::save_profile("test", &fixture.profile).unwrap();
        let path = configuration_path(&alias, &fixture.profile).unwrap();
        assert!(
            connect_with(
                &alias,
                &fixture.profile,
                |_, _| Ok(String::new()),
                |_, _| Err("up failed".into())
            )
            .is_err()
        );
        let saved = fs::read(&path).unwrap();
        let interface = interface_name(&fixture.profile.snapshot().network.id);
        let key = fixture.keys.wg_public_key().unwrap().to_string();
        let mut commands = vec![];
        assert!(
            connect_with(
                &alias,
                &fixture.profile,
                |_, args| {
                    Ok(if args == ["show", "interfaces"] {
                        interface.clone()
                    } else {
                        key.clone()
                    })
                },
                |_, args| {
                    commands.push(args[0].to_string());
                    Err("down failed".into())
                }
            )
            .is_err()
        );
        assert_eq!(commands, ["down"]);
        assert_eq!(fs::read(&path).unwrap(), saved);
        let mut wrong_alias = alias;
        wrong_alias.id = NetworkId::random().unwrap().to_string();
        assert!(configuration_path(&wrong_alias, &fixture.profile).is_err());
    }
}
