//! User-owned OpenSSH aliases for active system connections.
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::{Profile, Result};
use crate::config::LocalNetwork;
use tako_network::{LocalDeviceKeys, MemberKind};

fn directory(alias: &LocalNetwork) -> Result<PathBuf> {
    Ok(alias.profile_path()?.parent().unwrap().join("ssh"))
}

pub(crate) fn install(alias: &LocalNetwork, profile: &Profile) -> Result<()> {
    configure(alias, profile, true)
}

pub(crate) fn preflight(alias: &LocalNetwork, profile: &Profile) -> Result<()> {
    configure(alias, profile, false)
}

fn configure(alias: &LocalNetwork, profile: &Profile, write: bool) -> Result<()> {
    let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
    let own = profile
        .request()
        .device_id_for(&profile.snapshot().network.id)?;
    let permissions = profile.snapshot().permissions(&own, super::now()?)?;
    let Some(ref user) = permissions.ssh_user else {
        return if write { remove(alias) } else { Ok(()) };
    };
    let directory = directory(alias)?;
    super::private_directory(&directory)?;
    let identity = directory.join("identity");
    let known_hosts = directory.join("known_hosts");
    let (hosts, settings) = render(alias, profile, user.as_str(), &identity, &known_hosts)?;
    if write {
        super::write_bytes(&identity, keys.ssh_private_key()?.as_bytes(), true)?;
        super::write_bytes(&known_hosts, hosts.as_bytes(), true)?;
    }
    edit_ssh_config(&alias.id, Some(&settings), write)
}

fn render(
    alias: &LocalNetwork,
    profile: &Profile,
    user: &str,
    identity: &Path,
    known_hosts: &Path,
) -> Result<(String, String)> {
    let mut hosts = String::new();
    let mut settings = String::new();
    for (id, member) in super::server_members(profile)? {
        let MemberKind::Server(info) = &member.kind else {
            unreachable!()
        };
        let name = member.request.name.as_str().to_ascii_lowercase();
        let address = profile
            .snapshot()
            .network
            .id
            .address(&member.request.wg_public_key);
        let host_alias = format!("tako-{id}");
        hosts.push_str(&format!("{host_alias} {}\n", info.ssh_host_key));
        settings.push_str(&format!(
            "Host {name} {name}.{}.tako.internal\n  HostName {address}\n  User {user}\n  Port {}\n  IdentityFile {}\n  IdentitiesOnly yes\n  IdentityAgent none\n  HostKeyAlias {host_alias}\n  UserKnownHostsFile {}\n  GlobalKnownHostsFile /dev/null\n  StrictHostKeyChecking yes\n  UpdateHostKeys no\n  ProxyCommand none\n  ProxyJump none\n  ControlMaster no\n  ControlPath none\n  ForwardAgent no\n  ForwardX11 no\n",
            alias.id, info.ssh_port, quote_path(identity)?, quote_path(known_hosts)?,
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
    super::private_directory(&directory)?;
    let lock = super::private_file(&directory.join(".tako-config.lock"), true)?;
    lock.lock()?;
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
        let (_, settings) =
            render(&alias, &fixture.profile, "tako", &identity, &known_hosts).unwrap();
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
