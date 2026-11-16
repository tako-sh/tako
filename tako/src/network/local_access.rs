//! User-owned OpenSSH aliases for private servers.
//!
//! `tako connect` installs direct aliases that use the system interface.
//! `tako ssh config` installs proxied aliases that run the rootless tunnel and
//! share `tako ssh`'s control socket. Proxied aliases also work while
//! connected, so once opted in they survive connect and disconnect.
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::{Profile, Result};
use crate::config::LocalNetwork;
use tako_network::{LocalDeviceKeys, MemberKind};

/// Marks a network whose aliases should keep using the rootless proxy.
const PROXY_MARKER: &str = "proxy";

fn directory(alias: &LocalNetwork) -> Result<PathBuf> {
    Ok(alias.profile_path()?.parent().unwrap().join("ssh"))
}

fn proxied(alias: &LocalNetwork) -> Result<bool> {
    Ok(directory(alias)?.join(PROXY_MARKER).try_exists()?)
}

/// How OpenSSH reaches a server's private address.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Transport {
    /// Through the system WireGuard interface from `tako connect`.
    Direct,
    /// Through `tako __ssh-proxy`, with a control socket shared with `tako ssh`.
    Proxy,
}

pub(crate) fn install(alias: &LocalNetwork, profile: &Profile) -> Result<()> {
    let transport = if proxied(alias)? {
        Transport::Proxy
    } else {
        Transport::Direct
    };
    configure(alias, profile, transport, true).map(|_| ())
}

pub(crate) fn preflight(alias: &LocalNetwork, profile: &Profile) -> Result<()> {
    let transport = if proxied(alias)? {
        Transport::Proxy
    } else {
        Transport::Direct
    };
    configure(alias, profile, transport, false).map(|_| ())
}

/// Installs proxied aliases and returns the host names, primary name first.
pub(crate) fn install_proxy(
    alias: &LocalNetwork,
    profile: &Profile,
    write: bool,
) -> Result<Vec<String>> {
    let hosts = configure(alias, profile, Transport::Proxy, write)?;
    if write {
        super::write_bytes(&directory(alias)?.join(PROXY_MARKER), b"", true)?;
    }
    Ok(hosts)
}

/// Removes proxied aliases. While `connected`, direct aliases take their place.
pub(crate) fn remove_proxy(alias: &LocalNetwork, profile: &Profile, connected: bool) -> Result<()> {
    match fs::remove_file(directory(alias)?.join(PROXY_MARKER)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // Direct aliases need current membership; without it, removal still succeeds.
    if connected && configure(alias, profile, Transport::Direct, true).is_ok() {
        return Ok(());
    }
    remove(alias)
}

/// Removes direct aliases on disconnect. Proxied aliases keep working.
pub(crate) fn disconnect(alias: &LocalNetwork) -> Result<()> {
    if proxied(alias)? {
        return Ok(());
    }
    remove(alias)
}

fn configure(
    alias: &LocalNetwork,
    profile: &Profile,
    transport: Transport,
    write: bool,
) -> Result<Vec<String>> {
    let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
    let own = profile
        .request()
        .device_id_for(&profile.snapshot().network.id)?;
    let permissions = profile.snapshot().permissions(&own, super::now()?)?;
    let Some(ref user) = permissions.ssh_user else {
        if transport == Transport::Proxy {
            return Err("This device has no SSH permission".into());
        }
        return if write { remove(alias) } else { Ok(()) }.map(|_| vec![]);
    };
    let directory = directory(alias)?;
    if write {
        super::private_directory(&directory)?;
    }
    let identity = directory.join("identity");
    let known_hosts = directory.join("known_hosts");
    let proxy = match transport {
        Transport::Direct => None,
        Transport::Proxy => Some(proxy_executable()?),
    };
    let (hosts, settings) = render(
        alias,
        profile,
        user.as_str(),
        &identity,
        &known_hosts,
        proxy.as_deref(),
    )?;
    if write {
        super::write_bytes(&identity, keys.ssh_private_key()?.as_bytes(), true)?;
        super::write_bytes(&known_hosts, hosts.as_bytes(), true)?;
    }
    edit_ssh_config(&alias.id, Some(&settings), write)?;
    Ok(settings
        .lines()
        .filter_map(|line| line.strip_prefix("Host "))
        .filter_map(|names| names.split_whitespace().next())
        .map(str::to_owned)
        .collect())
}

/// A path to this executable that survives package-manager upgrades: the
/// `tako` on PATH when it is this binary (e.g. a Homebrew symlink), otherwise
/// the running executable.
fn proxy_executable() -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    let resolved = fs::canonicalize(&current)?;
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            let candidate = directory.join("tako");
            if candidate.is_absolute()
                && fs::canonicalize(&candidate).is_ok_and(|path| path == resolved)
            {
                return Ok(candidate);
            }
        }
    }
    Ok(current)
}

fn render(
    alias: &LocalNetwork,
    profile: &Profile,
    user: &str,
    identity: &Path,
    known_hosts: &Path,
    proxy: Option<&Path>,
) -> Result<(String, String)> {
    let mut hosts = String::new();
    let mut settings = String::new();
    let servers = super::server_members(profile)?;
    let named = |name: &str| {
        [
            name.to_owned(),
            format!("{name}.{}.tako.internal", alias.id),
        ]
    };
    let mut uses = std::collections::BTreeMap::<String, usize>::new();
    for member in servers.values() {
        for pattern in named(&member.request.name.as_str().to_ascii_lowercase()) {
            *uses.entry(pattern).or_default() += 1;
        }
    }
    for (id, member) in servers {
        let MemberKind::Server(info) = &member.kind else {
            unreachable!()
        };
        let name = member.request.name.as_str().to_ascii_lowercase();
        // OpenSSH applies the first matching stanza, so a pattern shared with
        // another server (the same name, or `edge` beside a server literally
        // named `edge.<network>.tako.internal`) would reach only one of them.
        // Those servers get their device ID, which `tako ssh` also accepts.
        let candidates = named(&name);
        let patterns = if candidates.iter().any(|pattern| uses[pattern] > 1) {
            id.to_string()
        } else {
            candidates.join(" ")
        };
        let address = profile
            .snapshot()
            .network
            .id
            .address(&member.request.wg_public_key);
        let host_alias = format!("tako-{id}");
        hosts.push_str(&format!("{host_alias} {}\n", info.ssh_host_key));
        let transport = match proxy {
            None => {
                "  ProxyCommand none\n  ProxyJump none\n  ControlMaster no\n  ControlPath none\n"
                    .to_owned()
            }
            Some(executable) => {
                let control = super::ssh_session::control_path(
                    alias,
                    &id.to_string(),
                    &super::ssh_session::settings(
                        user,
                        info.ssh_port,
                        &info.ssh_host_key.to_string(),
                        &address.to_string(),
                    ),
                )?;
                format!(
                    "  ProxyCommand {}\n  ProxyJump none\n  ControlMaster auto\n  ControlPath {}\n  ControlPersist 5\n",
                    super::ssh_tunnel::proxy_command(executable, &alias.id, &id.to_string())?,
                    quote_path(&control)?,
                )
            }
        };
        settings.push_str(&format!(
            "Host {patterns}\n  HostName {address}\n  User {user}\n  Port {}\n  IdentityFile {}\n  IdentitiesOnly yes\n  IdentityAgent none\n  HostKeyAlias {host_alias}\n  UserKnownHostsFile {}\n  GlobalKnownHostsFile /dev/null\n  StrictHostKeyChecking yes\n  UpdateHostKeys no\n{transport}  ForwardAgent no\n  ForwardX11 no\n",
            info.ssh_port, quote_path(identity)?, quote_path(known_hosts)?,
        ));
    }
    Ok((hosts, settings))
}

pub(crate) fn remove(alias: &LocalNetwork) -> Result<()> {
    edit_ssh_config(&alias.id, None, true)?;
    let directory = directory(alias)?;
    for name in ["identity", "known_hosts"] {
        match fs::remove_file(directory.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn quote_path(path: &Path) -> Result<String> {
    let value = path.to_str().ok_or("SSH paths must be UTF-8")?;
    if value.chars().any(char::is_control) {
        return Err("SSH paths cannot contain control characters".into());
    }
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}

fn edit_ssh_config(network: &str, settings: Option<&str>, write: bool) -> Result<()> {
    let directory = dirs::home_dir()
        .ok_or("Cannot find the SSH configuration directory")?
        .join(".ssh");
    if settings.is_none() && !directory.join("config").exists() {
        return Ok(());
    }
    // A preview only reads, so it must not create `~/.ssh` or the lock file.
    let _lock = if write {
        super::private_directory(&directory)?;
        let lock = super::private_file(&directory.join(".tako-config.lock"), true)?;
        lock.lock()?;
        Some(lock)
    } else {
        None
    };
    let path = directory.join("config");
    let mut previous = String::new();
    let mode = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(mut file) => {
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.nlink() != 1
                || metadata.mode() & 0o022 != 0
            {
                return Err(
                    "SSH config must be an owned regular file, not writable by other users".into(),
                );
            }
            (&mut file)
                .take(4 * 1024 * 1024 + 1)
                .read_to_string(&mut previous)?;
            if previous.len() > 4 * 1024 * 1024 {
                return Err("SSH config exceeds 4 MiB".into());
            }
            metadata.mode() & 0o777
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0o600,
        Err(error) => return Err(error.into()),
    };
    let contents = config_contents(&previous, network, settings)?;
    if !write || contents == previous {
        return Ok(());
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.write_all(contents.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}

fn config_contents(previous: &str, network: &str, settings: Option<&str>) -> Result<String> {
    let begin = format!("# BEGIN TAKO NETWORK {network}\n");
    let end = format!("# END TAKO NETWORK {network}\n");
    let mut rest = previous.to_string();
    if let Some(start) = rest.find(&begin) {
        let finish = rest[start..]
            .find(&end)
            .ok_or("Incomplete Tako SSH configuration block")?
            + start
            + end.len();
        rest.replace_range(start..finish, "");
    }
    if rest.contains(&begin) || rest.contains(&end) {
        return Err("Duplicate Tako SSH configuration block".into());
    }
    let Some(settings) = settings else {
        return Ok(rest);
    };
    let names: Vec<_> = settings
        .lines()
        .filter_map(|line| line.strip_prefix("Host "))
        .flat_map(str::split_whitespace)
        .collect();
    for line in rest.lines() {
        let line = line.split('#').next().unwrap_or("").trim_start();
        let Some((key, value)) = line.split_once(|c: char| c.is_ascii_whitespace() || c == '=')
        else {
            continue;
        };
        let patterns = value.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '=');
        if key.eq_ignore_ascii_case("Host")
            && patterns.split_whitespace().any(|word| {
                names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(word.trim_matches(['\'', '"'])))
            })
        {
            return Err(
                "A server alias already exists in your SSH config or another connected network"
                    .into(),
            );
        }
    }
    // Reset scope before returning to the user's existing global settings.
    Ok(format!("{begin}{settings}Host *\n{end}{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openssh_reads_the_approved_alias_without_leaking_settings_to_other_hosts() {
        let fixture = crate::network::tests::fixture();
        let alias = LocalNetwork {
            name: "test".into(),
            id: fixture.profile.snapshot().network.id.to_string(),
            device: "11".repeat(32),
        };
        let directory = tempfile::tempdir().unwrap();
        let identity = directory.path().join("device key %h");
        let known_hosts = directory.path().join("known hosts");
        let (_, settings) = render(
            &alias,
            &fixture.profile,
            "tako",
            &identity,
            &known_hosts,
            None,
        )
        .unwrap();
        let original = "Host *\n  User personal\n  ProxyCommand unwanted-proxy\n";
        let installed = config_contents(original, &alias.id, Some(&settings)).unwrap();
        let config = directory.path().join("config");
        fs::write(&config, installed).unwrap();
        let inspect = |host| {
            let result = std::process::Command::new("ssh")
                .args(["-G", "-F"])
                .arg(&config)
                .arg(host)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            String::from_utf8(result.stdout).unwrap()
        };
        let active = inspect("server");
        assert!(active.lines().any(|line| line == "user tako"));
        assert!(active.contains("\nstricthostkeychecking true\n"));
        assert!(active.contains(&format!("\nhostkeyalias tako-{}\n", fixture.server)));
        assert!(!active.contains("unwanted-proxy"));
        assert!(
            inspect("personal")
                .lines()
                .any(|line| line == "user personal")
        );
        assert!(config_contents("Host=\"server\"\n", &alias.id, Some(&settings)).is_err());
    }
    #[test]
    fn proxied_alias_runs_the_tunnel_and_shares_the_tako_ssh_control_socket() {
        let fixture = crate::network::tests::fixture();
        let alias = LocalNetwork {
            name: "test".into(),
            id: fixture.profile.snapshot().network.id.to_string(),
            device: "11".repeat(32),
        };
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("bin dir/tako");
        let (_, settings) = render(
            &alias,
            &fixture.profile,
            "tako",
            &directory.path().join("identity"),
            &directory.path().join("known_hosts"),
            Some(&executable),
        )
        .unwrap();
        let config = directory.path().join("config");
        fs::write(
            &config,
            config_contents("", &alias.id, Some(&settings)).unwrap(),
        )
        .unwrap();
        let result = std::process::Command::new("ssh")
            .args(["-G", "-F"])
            .arg(&config)
            .arg("server")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let resolved = String::from_utf8(result.stdout).unwrap();
        let value = |key: &str| {
            resolved
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{key} ")))
                .unwrap_or_else(|| panic!("missing {key}"))
                .to_owned()
        };
        assert_eq!(
            value("proxycommand"),
            format!(
                "'{}' '__ssh-proxy' '{}' '{}'",
                executable.display(),
                alias.id,
                fixture.server
            )
        );
        assert_eq!(value("controlmaster"), "auto");
        let members = crate::network::server_members(&fixture.profile).unwrap();
        let member = &members[&fixture.server];
        let MemberKind::Server(info) = &member.kind else {
            unreachable!()
        };
        let address = fixture
            .profile
            .snapshot()
            .network
            .id
            .address(&member.request.wg_public_key);
        let expected = crate::network::ssh_session::control_path(
            &alias,
            &fixture.server.to_string(),
            &crate::network::ssh_session::settings(
                "tako",
                info.ssh_port,
                &info.ssh_host_key.to_string(),
                &address.to_string(),
            ),
        )
        .unwrap();
        assert_eq!(value("controlpath"), expected.display().to_string());
        assert_eq!(value("forwardagent"), "no");
    }

    #[test]
    fn servers_sharing_a_name_get_distinct_device_id_aliases() {
        colliding_aliases(|_| "SERVER".into());
        // A dotted name can equal another server's generated private name.
        colliding_aliases(|network| format!("server.{network}.tako.internal"));
    }

    fn colliding_aliases(twin_name: impl Fn(&str) -> String) {
        let mut fixture = crate::network::tests::fixture();
        let Profile::Active { profile } = &mut fixture.profile else {
            panic!()
        };
        let keys = tako_network::LocalDeviceKeys::generate().unwrap();
        let request = keys
            .enrollment_request(
                Some(profile.snapshot.network.id.clone()),
                tako_network::DeviceKind::Server,
                twin_name(&profile.snapshot.network.id.to_string())
                    .parse()
                    .unwrap(),
            )
            .unwrap();
        let twin = request.device_id_for(&profile.snapshot.network.id).unwrap();
        let existing = crate::network::server_members(&fixture.profile).unwrap();
        let MemberKind::Server(info) = &existing[&fixture.server].kind else {
            unreachable!()
        };
        let mut info = info.clone();
        info.ssh_host_key = keys.ssh_public_key().unwrap();
        let Profile::Active { profile } = &mut fixture.profile else {
            panic!()
        };
        let admin = profile.snapshot.network.initial_admin.clone();
        crate::network::tests::add_member(
            &mut profile.snapshot,
            &keys,
            request,
            MemberKind::Server(info),
            Some(admin),
        );
        let alias = LocalNetwork {
            name: "test".into(),
            id: fixture.profile.snapshot().network.id.to_string(),
            device: "11".repeat(32),
        };
        let directory = tempfile::tempdir().unwrap();
        let (_, settings) = render(
            &alias,
            &fixture.profile,
            "tako",
            &directory.path().join("identity"),
            &directory.path().join("known_hosts"),
            None,
        )
        .unwrap();
        let mut patterns: Vec<_> = settings
            .lines()
            .filter_map(|line| line.strip_prefix("Host "))
            .collect();
        patterns.sort();
        let mut expected = vec![fixture.server.to_string(), twin.to_string()];
        expected.sort();
        assert_eq!(patterns, expected);
    }

    #[test]
    fn ssh_config_preserves_user_settings_and_other_networks() {
        let original = "Host personal\n  HostName example.com\nHost *\n  ServerAliveInterval 30\n";
        let settings = "Host foobar foobar.a.tako.internal\n  HostName fd42::1\n";
        let installed = config_contents(original, "a", Some(settings)).unwrap();
        assert!(installed.ends_with(original));
        assert_eq!(
            config_contents(&installed, "a", Some(settings)).unwrap(),
            installed
        );
        assert!(config_contents(&installed, "b", Some(settings)).is_err());
        let changed = format!("{installed}# later user edit\n");
        assert_eq!(
            config_contents(&changed, "a", None).unwrap(),
            format!("{original}# later user edit\n")
        );
    }
}
