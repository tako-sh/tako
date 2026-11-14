//! Workstation-local SSH selections, separate from shared network membership.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tako_network::DeviceId;

use super::Result;
use crate::config::LocalNetwork;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Access {
    pub(crate) host: Option<String>,
    pub(crate) key_path: Option<PathBuf>,
}

fn path(alias: &LocalNetwork, server: &DeviceId) -> Result<PathBuf> {
    Ok(alias
        .profile_path()?
        .with_file_name(format!("ssh-{server}.json")))
}

pub(crate) fn save(
    alias: &LocalNetwork,
    server: &DeviceId,
    host: &str,
    key: Option<&Path>,
) -> Result<()> {
    let key_path = key
        .map(|key| std::path::absolute(crate::ssh::expand_tilde(key)))
        .transpose()?;
    super::write_json(
        &path(alias, server)?,
        &Access {
            host: Some(host.to_owned()),
            key_path,
        },
        true,
    )
}

pub(crate) fn load(alias: &LocalNetwork, server: &DeviceId) -> Result<Access> {
    let access: Access = match super::read_json(&path(alias, server)?) {
        Ok(access) => access,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(Access::default());
        }
        Err(error) => return Err(error),
    };
    if access
        .key_path
        .as_ref()
        .is_some_and(|key| !key.is_absolute())
    {
        return Err("Saved server SSH key path must be absolute".into());
    }
    Ok(access)
}
