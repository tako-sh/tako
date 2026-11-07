//! Split DNS for services and scoped hosts entries for short server names.
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, Write};
#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use super::{Configuration, Result, ServerName, network};

pub(super) fn validate(
    network: &str,
    logical: &str,
    names: &[ServerName],
    config: &Configuration,
) -> Result<()> {
    let id: tako_network::NetworkId = network.parse()?;
    if super::super::interface_name(&id) != logical {
        return Err("Resolver network does not match the tunnel".into());
    }
    let mut unique = BTreeSet::new();
    for record in names {
        let _: tako_network::DeviceName = record.name.parse()?;
        if !unique.insert(record.name.to_ascii_lowercase()) {
            return Err("Server names must be unique to connect using short names".into());
        }
        if !config
            .peers
            .iter()
            .any(|peer| peer.address == record.address)
        {
            return Err("Resolver address is not an approved server route".into());
        }
    }
    Ok(())
}

pub(super) fn preflight() -> Result<()> {
    #[cfg(target_os = "linux")]
    super::run_tool("resolvectl", &["status"])
        .map_err(|_| "Private DNS requires an active systemd-resolved service and resolvectl. Use `tako ssh` for SSH without changing system networking.")?;
    #[cfg(target_os = "linux")]
    {
        let resolver = fs::read_to_string("/etc/resolv.conf")?;
        let uses_stub = resolver.lines().any(|line| {
            let words: Vec<_> = line.split_whitespace().collect();
            words.first() == Some(&"nameserver")
                && matches!(words.get(1), Some(&"127.0.0.53" | &"127.0.0.54"))
        });
        let uses_nss = fs::read_to_string("/etc/nsswitch.conf")
            .unwrap_or_default()
            .lines()
            .any(nss_uses_resolved);
        if !uses_stub && !uses_nss {
            return Err("The system resolver does not use systemd-resolved. Enable its DNS stub or nss-resolve before connecting, or use `tako ssh`.".into());
        }
    }
    Ok(())
}

pub(super) fn install(
    network: &str,
    logical: &str,
    actual: &str,
    names: &[ServerName],
    config: &Configuration,
) -> Result<()> {
    let state = super::runtime_directory()?.join(format!("{logical}.dns"));
    network::write_bytes(&state, network.as_bytes(), true)?;
    let domain = format!("{network}.tako.internal");
    #[cfg(target_os = "linux")]
    {
        let addresses: Vec<_> = config
            .peers
            .iter()
            .map(|peer| peer.address.to_string())
            .collect();
        let mut args = vec!["dns", actual];
        args.extend(addresses.iter().map(String::as_str));
        super::run_tool("resolvectl", &args)?;
        super::run_tool("resolvectl", &["domain", actual, &domain])?;
        super::run_tool("resolvectl", &["default-route", actual, "no"])?;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = actual;
        let directory = fs::canonicalize("/etc")?.join("resolver");
        match fs::create_dir(&directory) {
            Ok(()) => fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err("The resolver directory is not owned exclusively by root".into());
        }
        let path = directory.join(&domain);
        let header = format!("# Managed by Tako: {network}\n");
        if let Some(previous) = read_resolver(&path)? {
            if !previous.starts_with(&header) {
                return Err("The network resolver file is managed outside Tako".into());
            }
        }
        let mut contents = header;
        for peer in &config.peers {
            contents.push_str(&format!("nameserver {}\n", peer.address));
        }
        contents.push_str(&format!("domain {domain}\ntimeout 2\n"));
        // Resolver configuration is public and must be readable by system DNS
        // clients. The directory and file remain writable only by root.
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
        temporary.write_all(contents.as_bytes())?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
    }
    update_hosts(Path::new("/etc/hosts"), network, names)?;
    Ok(())
}

pub(super) fn cleanup(logical: &str) -> Result<()> {
    let state = super::runtime_directory()?.join(format!("{logical}.dns"));
    let file = match network::private_file(&state, false) {
        Ok(file) => file,
        Err(error) if super::is_not_found(error.as_ref()) => return Ok(()),
        Err(error) => return Err(error),
    };
    let bytes = super::read_bounded(file)?;
    let network = std::str::from_utf8(&bytes)?;
    let id: tako_network::NetworkId = network.parse()?;
    if super::super::interface_name(&id) != logical {
        return Err("Saved resolver network does not match the tunnel".into());
    }
    #[cfg(target_os = "macos")]
    {
        let path = fs::canonicalize("/etc")?
            .join("resolver")
            .join(format!("{network}.tako.internal"));
        match read_resolver(&path)? {
            Some(contents) => {
                if !contents.starts_with(&format!("# Managed by Tako: {network}\n")) {
                    return Err("The network resolver was modified outside Tako".into());
                }
                fs::remove_file(path)?;
            }
            None => {}
        }
    }
    update_hosts(Path::new("/etc/hosts"), network, &[])?;
    super::remove_name(&state)
}

#[cfg(any(target_os = "linux", test))]
fn nss_uses_resolved(line: &str) -> bool {
    line.split('#')
        .next()
        .unwrap_or("")
        .trim_start()
        .strip_prefix("hosts:")
        .is_some_and(|hosts| hosts.split_whitespace().any(|word| word == "resolve"))
}

#[cfg(target_os = "macos")]
fn read_resolver(path: &Path) -> Result<Option<String>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.nlink() != 1
        || metadata.mode() & 0o022 != 0
    {
        return Err(
            "The network resolver file must be owned by root and not writable by others".into(),
        );
    }
    let mut contents = String::new();
    file.take(65537).read_to_string(&mut contents)?;
    if contents.len() > 65536 {
        return Err("The network resolver file exceeds 64 KiB".into());
    }
    Ok(Some(contents))
}

fn update_hosts(path: &Path, network: &str, names: &[ServerName]) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    file.lock()?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err("The hosts file must be owned by root and not writable by others".into());
    }
    let mut previous = String::new();
    (&mut file)
        .take(4 * 1024 * 1024 + 1)
        .read_to_string(&mut previous)?;
    if previous.len() > 4 * 1024 * 1024 {
        return Err("The hosts file exceeds 4 MiB".into());
    }
    let contents = hosts_contents(&previous, network, names)?;
    if contents != previous {
        file.rewind()?;
        file.write_all(contents.as_bytes())?;
        file.set_len(contents.len() as u64)?;
        file.sync_all()?;
    }
    Ok(())
}

fn hosts_contents(previous: &str, network: &str, names: &[ServerName]) -> Result<String> {
    let begin = format!("# BEGIN TAKO NETWORK {network}\n");
    let end = format!("# END TAKO NETWORK {network}\n");
    let mut contents = previous.to_string();
    if let Some(start) = contents.find(&begin) {
        let finish = contents[start..]
            .find(&end)
            .ok_or("Incomplete Tako hosts block")?
            + start
            + end.len();
        contents.replace_range(start..finish, "");
    }
    if contents.contains(&begin) || contents.contains(&end) {
        return Err("Duplicate or incomplete Tako hosts block".into());
    }
    for record in names {
        let aliases = [
            record.name.clone(),
            format!("{}.{network}.tako.internal", record.name),
        ];
        for line in contents.lines() {
            let words: Vec<_> = line
                .split('#')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .collect();
            if words
                .iter()
                .skip(1)
                .any(|word| aliases.iter().any(|name| name.eq_ignore_ascii_case(word)))
            {
                return Err(format!("The server name '{}' already exists in the hosts file or another active network", record.name).into());
            }
        }
    }
    if !names.is_empty() {
        if !contents.is_empty() && !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str(&begin);
        for record in names {
            contents.push_str(&format!(
                "{} {} {}.{network}.tako.internal\n",
                record.address, record.name, record.name
            ));
        }
        contents.push_str(&end);
    }
    Ok(contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_nss_resolver_does_not_pass_preflight() {
        assert!(nss_uses_resolved(
            "  hosts: files resolve [!UNAVAIL=return] dns"
        ));
        assert!(!nss_uses_resolved("hosts: files dns # resolve disabled"));
        assert!(!nss_uses_resolved("# hosts: files resolve"));
    }
    #[test]
    fn hosts_are_scoped_idempotent_and_removed_without_losing_other_entries() {
        let original = "127.0.0.1 localhost\n# user's entries\n192.0.2.1 existing\n";
        let records = [ServerName {
            name: "foobar".into(),
            address: "fd42::1".parse().unwrap(),
        }];
        let installed = hosts_contents(original, "a", &records).unwrap();
        assert!(installed.contains("fd42::1 foobar foobar.a.tako.internal"));
        assert_eq!(
            hosts_contents(&installed, "a", &records).unwrap(),
            installed
        );
        assert!(hosts_contents(&installed, "b", &records).is_err());
        let modified = format!("{installed}192.0.2.2 added-later\n");
        assert_eq!(
            hosts_contents(&modified, "a", &[]).unwrap(),
            format!("{original}192.0.2.2 added-later\n")
        );
    }
}
