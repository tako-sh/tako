//! A client identity: a joined device's keys and network profile in one
//! string, for machines that keep no state between runs, such as CI.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::{Profile, Result};
use crate::config::LocalNetwork;

pub(crate) const ENV: &str = "TAKO_CLIENT_IDENTITY";
const PREFIX: &str = "tako-identity-v1-";
/// GitHub limits a secret to 48 KiB.
const MAX_BYTES: usize = 48 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Contents {
    profile: Profile,
    /// The device's key file, as `LocalDeviceKeys` stores it.
    keys: Zeroizing<String>,
}

pub(crate) fn is_identity(value: &str) -> bool {
    value.trim().starts_with("tako-identity-")
}

/// Package an active joined device as an identity.
pub(crate) fn encode(alias: &LocalNetwork) -> Result<Zeroizing<String>> {
    let profile = super::load_profile(alias)?;
    if !matches!(profile, Profile::Active { .. }) {
        return Err("The new client has not been admitted yet".into());
    }
    let keys = Zeroizing::new(std::fs::read_to_string(super::keys_path(
        &profile.request().request_id,
    )?)?);
    let json = Zeroizing::new(serde_json::to_vec(&Contents { profile, keys })?);
    let identity = Zeroizing::new(format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&*json)
    ));
    if identity.len() > MAX_BYTES {
        return Err(format!(
            "The identity is {} KiB, more than the 48 KiB a GitHub secret holds. Use an invitation instead.",
            identity.len() / 1024
        )
        .into());
    }
    Ok(identity)
}

/// Store an identity in the current network state, like a joined device.
pub(crate) fn store(value: &str) -> Result<LocalNetwork> {
    let encoded = value
        .trim()
        .strip_prefix(PREFIX)
        .ok_or("Not a Tako client identity, or one from a newer Tako version")?;
    if encoded.len() > MAX_BYTES * 2 {
        return Err("Client identity is too large".into());
    }
    let json = Zeroizing::new(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| "Client identity is malformed")?,
    );
    let contents: Contents =
        serde_json::from_slice(&json).map_err(|_| "Client identity is malformed")?;
    let path = super::keys_path(&contents.profile.request().request_id)?;
    super::private_directory(path.parent().ok_or("Invalid key path")?)?;
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing == *contents.keys => {}
        Ok(_) => return Err("A different key is already stored for this identity".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            super::write_bytes(&path, contents.keys.as_bytes(), false)?;
        }
        Err(error) => return Err(error.into()),
    }
    // Saving checks that the profile and the keys belong to the same device.
    super::save_profile(&contents.profile)
}

/// While alive, this process keeps its network state in a private temporary
/// directory instead of the Tako home. Dropping it removes the directory.
pub(crate) struct TemporaryState {
    _directory: tempfile::TempDir,
}

impl TemporaryState {
    pub(crate) fn new() -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("tako-identity-")
            .tempdir()?;
        super::private_directory(directory.path())?;
        crate::paths::set_network_state_dir(Some(directory.path().to_path_buf()));
        Ok(Self {
            _directory: directory,
        })
    }
}

/// The temporary directory is removed when `_directory` drops, after this.
impl Drop for TemporaryState {
    fn drop(&mut self) {
        crate::paths::set_network_state_dir(None);
    }
}

/// Use `TAKO_CLIENT_IDENTITY` for this process when it is set. The keys stay
/// in a private temporary directory and never reach the Tako home.
pub(crate) fn activate_from_env() -> Result<Option<TemporaryState>> {
    let Some(value) = std::env::var_os(ENV) else {
        return Ok(None);
    };
    let value = Zeroizing::new(
        value
            .into_string()
            .map_err(|_| format!("{ENV} is not valid text"))?,
    );
    if value.trim().is_empty() {
        return Ok(None);
    }
    let state = TemporaryState::new()?;
    store(&value).map_err(|error| format!("{ENV}: {error}"))?;
    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_and_invitations_are_told_apart_by_prefix() {
        assert!(is_identity("  tako-identity-v1-abc"));
        assert!(!is_identity("tako-invite-abc"));
        let error = store("tako-identity-v9-abc").unwrap_err().to_string();
        assert!(error.contains("newer Tako version"), "{error}");
        let error = store("tako-identity-v1-!!!").unwrap_err().to_string();
        assert!(error.contains("malformed"), "{error}");
    }
}
