use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tako_network::{
    DeviceKind, EnrollmentRequest, LocalDeviceKeys, NetworkId, NetworkPublicProfile,
    PendingApprovalProfile, RegistrySnapshot, RequestId,
};

use crate::config::{NetworkAlias, NetworksToml};

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

    pub fn validate(&self, keys: &LocalDeviceKeys) -> Result<()> {
        let now = now()?;
        match self {
            Self::Active { profile } => profile.validate_for(keys, now)?,
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
    let profile: Profile = read_json(&network.profile_path()?)?;
    if profile.snapshot().network.id.to_string() != network.id
        || profile
            .request()
            .device_id_for(&profile.snapshot().network.id)?
            .to_string()
            != network.device
    {
        return Err("Saved network alias does not match its profile".into());
    }
    let keys = LocalDeviceKeys::load(&keys_path(&profile.request().request_id)?)?;
    profile.validate(&keys)?;
    Ok(profile)
}

pub(crate) fn save_profile(name: &str, profile: &Profile) -> Result<NetworkAlias> {
    let keys = LocalDeviceKeys::load(&keys_path(&profile.request().request_id)?)?;
    profile.validate(&keys)?;
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
    write_json(&path, profile, true)?;
    NetworksToml::save_alias(&alias)?;
    Ok(alias)
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err("Network directory must be owned by the current user".into());
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)?;
    Ok(())
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("Network file exceeds 4 MiB".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn write_json(path: &Path, value: &impl Serialize, replace: bool) -> Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temporary, value)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    if replace {
        if fs::symlink_metadata(path)
            .is_ok_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink())
        {
            return Err("Refusing to replace a non-regular network file".into());
        }
        temporary.persist(path)?;
    } else {
        temporary.persist_noclobber(path)?;
    }
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
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
    if let Ok(context) = crate::commands::project_context::resolve(config_path)
        && let Ok(project) = crate::config::TakoToml::load_from_file(context.config_path)
        && let Some(network) = project
            .envs
            .get("production")
            .and_then(|env| env.network.as_deref())
    {
        let _: NetworkId = network.parse()?;
        return Ok(aliases.select(Some(network))?.clone());
    }
    Ok(aliases.select(None)?.clone())
}
