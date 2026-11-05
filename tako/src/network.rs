use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tako_network::{
    DeviceKind, EnrollmentRequest, LocalDeviceKeys, NetworkId, NetworkPublicProfile,
    PendingApprovalProfile, RegistrySnapshot, RequestId,
};

use crate::config::{NetworkAlias, NetworksToml};

pub(crate) mod client;
pub(crate) mod wireguard;

pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Profile {
    Created {
        request: EnrollmentRequest,
        snapshot: RegistrySnapshot,
    },
    Pending {
        profile: PendingApprovalProfile,
    },
    Active {
        profile: NetworkPublicProfile,
    },
}

impl Profile {
    pub fn request(&self) -> &EnrollmentRequest {
        match self {
            Self::Created { request, .. } => request,
            Self::Pending { profile } => &profile.request,
            Self::Active { profile } => &profile.request,
        }
    }

    pub fn snapshot(&self) -> &RegistrySnapshot {
        match self {
            Self::Created { snapshot, .. } => snapshot,
            Self::Pending { profile } => &profile.snapshot,
            Self::Active { profile } => &profile.snapshot,
        }
    }

    fn normalize_trusted_servers(&mut self) -> Result<()> {
        if let Self::Active { profile } = self {
            profile.trusted_servers = current_server_pins(profile, now()?)?;
        }
        Ok(())
    }

    fn validate_for_storage(&self, keys: &LocalDeviceKeys) -> Result<()> {
        let Self::Active { profile } = self else {
            return self.validate(keys);
        };
        profile.request.verify()?;
        let id = profile
            .request
            .device_id_for(&profile.snapshot.network.id)?;
        if profile.request.wg_public_key != keys.wg_public_key()?
            || profile.request.ssh_public_key != keys.ssh_public_key()?
            || !profile
                .snapshot
                .members
                .iter()
                .any(|member| member.device_id == id && member.request == profile.request)
        {
            return Err("Network profile does not match this device".into());
        }
        let servers = server_members_from_snapshot(&profile.snapshot)?;
        if servers != profile.trusted_servers {
            return Err("Network profile server pins do not match its membership".into());
        }
        Ok(())
    }

    pub fn validate(&self, keys: &LocalDeviceKeys) -> Result<()> {
        let now = now()?;
        match self {
            Self::Active { profile } => {
                let mut current = profile.clone();
                current.trusted_servers = current_server_pins(profile, now)?;
                current.validate_for(keys, now)?;
            }
            Self::Pending { profile } => profile.validate_for(keys, now)?,
            Self::Created { request, snapshot } => {
                request.verify()?;
                snapshot.validate()?;
                if request.kind != DeviceKind::Laptop
                    || request.wg_public_key != keys.wg_public_key()?
                    || request.ssh_public_key != keys.ssh_public_key()?
                    || request.device_id_for(&snapshot.network.id)?
                        != snapshot.network.initial_admin
                    || snapshot.members.len() != 1
                    || snapshot.effective_members(now)?.len() != 1
                {
                    return Err("Network creation profile does not match this device".into());
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

pub(crate) fn keys_path(request: &RequestId) -> Result<PathBuf> {
    Ok(crate::paths::tako_config_dir()?
        .join("network-keys")
        .join(format!("{request}.json")))
}

pub(crate) fn store_keys(request: &RequestId, keys: &LocalDeviceKeys) -> Result<()> {
    let path = keys_path(request)?;
    private_directory(path.parent().unwrap())?;
    keys.save_new(&path)?;
    Ok(())
}

pub(crate) fn load_profile(network: &NetworkAlias) -> Result<Profile> {
    let mut profile: Profile = read_json(&network.profile_path()?)?;
    if profile.snapshot().network.id.to_string() != network.id
        || profile
            .request()
            .device_id_for(&profile.snapshot().network.id)?
            .to_string()
            != network.device
    {
        return Err("Saved network alias does not match its profile".into());
    }
    profile.normalize_trusted_servers()?;
    let keys = LocalDeviceKeys::load(&keys_path(&profile.request().request_id)?)?;
    profile.validate(&keys)?;
    Ok(profile)
}

pub(crate) fn save_profile(name: &str, profile: &Profile) -> Result<NetworkAlias> {
    let mut profile = profile.clone();
    profile.normalize_trusted_servers()?;
    let keys = LocalDeviceKeys::load(&keys_path(&profile.request().request_id)?)?;
    profile.validate_for_storage(&keys)?;
    let alias = NetworkAlias {
        name: name.into(),
        id: profile.snapshot().network.id.to_string(),
        device: profile
            .request()
            .device_id_for(&profile.snapshot().network.id)?
            .to_string(),
    };
    alias.validate()?;
    if let Some(existing) = NetworksToml::load()?.networks.get(name)
        && existing.id != alias.id
    {
        return Err(format!("Alias '{name}' already names another network").into());
    }
    let path = alias.profile_path()?;
    private_directory(path.parent().unwrap())?;
    let lock = private_file(&path.with_file_name("profile.lock"), true)?;
    lock.lock()?;
    match read_json::<Profile>(&path) {
        Ok(previous) => {
            previous.snapshot().validate()?;
            if let Profile::Active { profile: active } = &previous {
                current_server_pins(active, now()?)?;
            }
            if previous.request() != profile.request() {
                return Err("Saved profile belongs to a different enrollment request".into());
            }
            match &mut profile {
                Profile::Active { profile: active } => {
                    merge_snapshot(&mut active.snapshot, previous.snapshot())?;
                    active.trusted_servers = server_members_from_snapshot(&active.snapshot)?;
                }
                Profile::Pending { profile: pending } => {
                    if matches!(previous, Profile::Active { .. }) {
                        return Err(
                            "Cannot replace an active profile with a pending approval".into()
                        );
                    }
                    merge_snapshot(&mut pending.snapshot, previous.snapshot())?;
                    pending.trusted_servers = server_members_from_snapshot(&pending.snapshot)?;
                }
                Profile::Created { snapshot, .. } => merge_snapshot(snapshot, previous.snapshot())?,
            }
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    profile.validate_for_storage(&keys)?;
    write_json(&path, &profile, true)?;
    // Persist authenticated revocations even when they remove this device or its last server.
    profile.validate(&keys)?;
    NetworksToml::save_alias(&alias)?;
    Ok(alias)
}

fn current_server_pins(
    profile: &NetworkPublicProfile,
    now: u64,
) -> Result<BTreeMap<tako_network::DeviceId, tako_network::ServerInfo>> {
    let effective: BTreeMap<_, _> = profile
        .snapshot
        .effective_members(now)?
        .into_iter()
        .filter_map(|(id, member)| match &member.kind {
            tako_network::MemberKind::Server(info) => Some((id, info.clone())),
            _ => None,
        })
        .collect();
    if effective.iter().any(|(id, info)| profile.trusted_servers.get(id) != Some(info))
        || profile.trusted_servers.iter().any(|(id, info)| !profile.snapshot.members.iter().any(|member| {
            member.wg_verified && &member.device_id == id && matches!(&member.kind, tako_network::MemberKind::Server(saved) if saved == info)
        }))
    {
        return Err("Network profile server pins do not match its membership".into());
    }
    Ok(effective)
}

fn merge_snapshot(snapshot: &mut RegistrySnapshot, previous: &RegistrySnapshot) -> Result<()> {
    if snapshot.network != previous.network {
        return Err("Server returned a different network identity".into());
    }
    for member in &previous.members {
        if !snapshot.members.contains(member) {
            snapshot.members.push(member.clone());
        }
    }
    for revocation in &previous.revocations {
        if !snapshot.revocations.contains(revocation) {
            snapshot.revocations.push(revocation.clone());
        }
    }
    for ack in &previous.acks {
        if !snapshot.acks.contains(ack) {
            snapshot.acks.push(ack.clone());
        }
    }
    snapshot.validate()?;
    Ok(())
}

fn server_members_from_snapshot(
    snapshot: &RegistrySnapshot,
) -> Result<BTreeMap<tako_network::DeviceId, tako_network::ServerInfo>> {
    Ok(snapshot
        .effective_members(now()?)?
        .into_iter()
        .filter_map(|(id, member)| match &member.kind {
            tako_network::MemberKind::Server(info) => Some((id, info.clone())),
            _ => None,
        })
        .collect())
}

#[cfg(unix)]
mod persistence {
    use super::{Path, Result};
    use std::ffi::CString;
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    use std::path::Component;

    fn name(value: &std::ffi::OsStr) -> Result<CString> {
        Ok(CString::new(value.as_bytes())?)
    }

    fn open_at(dir: &File, name: &CString, flags: i32) -> Result<File> {
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn directory(path: &Path, create: bool) -> Result<File> {
        let mut dir = File::open(if path.is_absolute() { "/" } else { "." })?;
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(part) => {
                    let part = name(part)?;
                    if create {
                        let result =
                            unsafe { libc::mkdirat(dir.as_raw_fd(), part.as_ptr(), 0o700) };
                        if result < 0 {
                            let error = std::io::Error::last_os_error();
                            if error.kind() != std::io::ErrorKind::AlreadyExists {
                                return Err(error.into());
                            }
                        }
                    }
                    dir = open_at(&dir, &part, libc::O_RDONLY | libc::O_DIRECTORY)?;
                }
                _ => return Err("Network paths cannot contain parent traversal".into()),
            }
        }
        Ok(dir)
    }

    fn parent(path: &Path, create: bool) -> Result<(File, CString)> {
        let dir = directory(path.parent().unwrap_or(Path::new(".")), create)?;
        let name = name(path.file_name().ok_or("Network path needs a filename")?)?;
        Ok((dir, name))
    }

    pub(super) fn private_directory(path: &Path) -> Result<()> {
        let dir = directory(path, true)?;
        if dir.metadata()?.uid() != unsafe { libc::geteuid() } {
            return Err("Network directory must be owned by the current user".into());
        }
        if unsafe { libc::fchmod(dir.as_raw_fd(), 0o700) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn check(file: &File, private: bool) -> Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err("Network file must be regular with exactly one link".into());
        }
        if private
            && (metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o7777 != 0o600)
        {
            return Err(
                "Private network file must be owned by the current user with mode 0600".into(),
            );
        }
        Ok(())
    }

    pub(super) fn open(path: &Path, private: bool, create: bool) -> Result<File> {
        let (dir, name) = parent(path, false)?;
        if private {
            let metadata = dir.metadata()?;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
                return Err("Private network directory must be owned by the current user and not writable by others".into());
            }
        }
        let flags = if create {
            libc::O_RDWR | libc::O_CREAT
        } else {
            libc::O_RDONLY
        };
        let file = open_at(&dir, &name, flags | libc::O_NONBLOCK)?;
        check(&file, private)?;
        Ok(file)
    }

    pub(super) fn write(path: &Path, bytes: &[u8], replace: bool) -> Result<()> {
        let (dir, destination) = parent(path, true)?;
        match open_at(&dir, &destination, libc::O_RDONLY | libc::O_NONBLOCK) {
            Ok(file) => {
                check(&file, false)?;
                if !replace {
                    return Err("Network output already exists".into());
                }
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error),
        }
        let temporary = CString::new(format!(
            ".tako-network-{}",
            tako_network::RequestId::random()?
        ))?;
        let mut file = open_at(
            &dir,
            &temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )?;
        let result = (|| -> Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            let result = unsafe {
                if replace {
                    libc::renameat(
                        dir.as_raw_fd(),
                        temporary.as_ptr(),
                        dir.as_raw_fd(),
                        destination.as_ptr(),
                    )
                } else {
                    libc::linkat(
                        dir.as_raw_fd(),
                        temporary.as_ptr(),
                        dir.as_raw_fd(),
                        destination.as_ptr(),
                        0,
                    )
                }
            };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        })();
        let cleanup = unsafe { libc::unlinkat(dir.as_raw_fd(), temporary.as_ptr(), 0) };
        result?;
        if !replace && cleanup < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        dir.sync_all()?;
        Ok(())
    }
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    return persistence::private_directory(path);
    #[cfg(not(unix))]
    {
        let _ = path;
        Err("Native network persistence requires Unix".into())
    }
}

pub(crate) fn private_file(path: &Path, create: bool) -> Result<fs::File> {
    #[cfg(unix)]
    return persistence::open(path, true, create);
    #[cfg(not(unix))]
    {
        let _ = (path, create);
        Err("Native network persistence requires Unix".into())
    }
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    #[cfg(unix)]
    let file = persistence::open(path, false, false)?;
    #[cfg(not(unix))]
    let file = fs::File::open(path)?;
    read_bounded_json(file)
}

pub(crate) fn read_bounded_json<T: serde::de::DeserializeOwned>(reader: impl Read) -> Result<T> {
    let mut bytes = Vec::new();
    reader.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("Network response or file exceeds 4 MiB".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn write_bytes(path: &Path, bytes: &[u8], replace: bool) -> Result<()> {
    #[cfg(unix)]
    return persistence::write(path, bytes, replace);
    #[cfg(not(unix))]
    {
        let _ = (path, bytes, replace);
        Err("Native network persistence requires Unix".into())
    }
}

pub(crate) fn write_json(path: &Path, value: &impl Serialize, replace: bool) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("Network file exceeds 4 MiB".into());
    }
    write_bytes(path, &bytes, replace)
}

pub(crate) fn server_members(
    profile: &Profile,
) -> Result<BTreeMap<tako_network::DeviceId, tako_network::Member>> {
    Ok(profile
        .snapshot()
        .effective_members(now()?)?
        .into_iter()
        .filter(|(_, member)| matches!(member.kind, tako_network::MemberKind::Server(_)))
        .map(|(id, member)| (id, member.clone()))
        .collect())
}

pub(crate) fn resolve_network(
    value: Option<&str>,
    config_path: Option<&Path>,
) -> Result<NetworkAlias> {
    let aliases = NetworksToml::load()?;
    if value.is_some() {
        return Ok(aliases.select(value)?.clone());
    }
    if let Some(context) = crate::commands::project_context::resolve_optional(config_path)? {
        let project = crate::config::TakoToml::load_from_file(context.config_path)?;
        if let Some(network) = project
            .envs
            .get("production")
            .and_then(|env| env.network.as_deref())
        {
            let _: NetworkId = network.parse()?;
            return Ok(aliases.select(Some(network))?.clone());
        }
    }
    Ok(aliases.select(None)?.clone())
}

#[cfg(test)]
mod tests;
