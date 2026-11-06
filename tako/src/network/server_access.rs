//! Workstation-local SSH selections, separate from shared network membership.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tako_network::DeviceId;

use super::Result;
use crate::config::NetworkAlias;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Access {
    key_path: Option<PathBuf>,
}

fn path(alias: &NetworkAlias, server: &DeviceId) -> Result<PathBuf> {
    Ok(alias
        .profile_path()?
        .with_file_name(format!("ssh-{server}.json")))
}

pub(crate) fn save_key(alias: &NetworkAlias, server: &DeviceId, key: Option<&Path>) -> Result<()> {
    let key_path = key
        .map(|key| std::path::absolute(crate::ssh::expand_tilde(key)))
        .transpose()?;
    super::write_json(&path(alias, server)?, &Access { key_path }, true)
}

pub(crate) fn load_key(alias: &NetworkAlias, server: &DeviceId) -> Result<Option<PathBuf>> {
    let access: Access = match super::read_json(&path(alias, server)?) {
        Ok(access) => access,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
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
    Ok(access.key_path)
}
