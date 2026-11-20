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

/// Store an identity in the current network state, like a joined device,
/// naming its network `alias` on this machine.
pub(crate) fn store(value: &str, alias: Option<&str>) -> Result<LocalNetwork> {
    store_keys_and_profile(value, alias).map(|(local, _)| local)
}

/// The network an identity belongs to, without storing anything.
pub(crate) fn network_of(value: &str) -> Result<tako_network::NetworkDescriptor> {
    Ok(decode(value)?.profile.snapshot().network.clone())
}

fn decode(value: &str) -> Result<Contents> {
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
    Ok(serde_json::from_slice(&json).map_err(|_| "Client identity is malformed")?)
}

/// [`store`], also returning the path of the stored key file.
fn store_keys_and_profile(
    value: &str,
    alias: Option<&str>,
) -> Result<(LocalNetwork, std::path::PathBuf)> {
    let contents = decode(value)?;
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
    Ok((super::save_profile_as(&contents.profile, alias)?, path))
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

/// `TAKO_CLIENT_IDENTITY` as the process started, once [`take_from_env`] ran.
static TAKEN: std::sync::OnceLock<Option<std::ffi::OsString>> = std::sync::OnceLock::new();

/// Move `TAKO_CLIENT_IDENTITY` out of the environment, so build, install and
/// other child commands never inherit the identity's private keys.
///
/// # Safety
///
/// Call before any other thread starts: it changes the process environment.
pub(crate) unsafe fn take_from_env() {
    let value = std::env::var_os(ENV);
    if value.is_some() {
        // SAFETY: the caller guarantees no other thread reads the environment.
        unsafe { std::env::remove_var(ENV) };
    }
    let _ = TAKEN.set(value);
}

/// `TAKO_CLIENT_IDENTITY` as this process received it, for passing on to a child
/// `tako` process only, never to project code.
pub(crate) fn from_env() -> Option<std::ffi::OsString> {
    match TAKEN.get() {
        Some(taken) => taken.clone(),
        None => std::env::var_os(ENV),
    }
}

/// Use `TAKO_CLIENT_IDENTITY` for this process when it is set. The keys stay
/// in a private temporary directory and never reach the Tako home.
pub(crate) fn activate_from_env() -> Result<Option<TemporaryState>> {
    let value = from_env();
    let Some(value) = value else {
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
    let (_, keys) =
        store_keys_and_profile(&value, None).map_err(|error| format!("{ENV}: {error}"))?;
    *ACTIVE_KEYS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(keys);
    Ok(Some(state))
}

/// The key file of the identity [`activate_from_env`] stored for this process.
static ACTIVE_KEYS: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// Whether this process acts as a client identity from `TAKO_CLIENT_IDENTITY`.
pub(crate) fn is_active() -> bool {
    ACTIVE_KEYS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_some()
}

/// Run `project_code` with the active identity's key file off disk, so code
/// from the project, such as a build script, cannot copy the identity's keys
/// while it runs. Without an identity, this only runs `project_code`.
pub(crate) fn without_keys<T>(project_code: impl FnOnce() -> T) -> Result<T> {
    let suspended = suspend_keys()?;
    let value = project_code();
    if let Some(suspended) = suspended {
        suspended.resume()?;
    }
    Ok(value)
}

/// Take the active identity's key file off disk until the returned guard
/// resumes or drops. `None` when no identity is active.
pub(crate) fn suspend_keys() -> Result<Option<SuspendedKeys>> {
    let path = ACTIVE_KEYS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    path.map(SuspendedKeys::take).transpose()
}

/// A key file taken off disk. [`SuspendedKeys::resume`] writes it back; so
/// does dropping the guard on an early return.
#[must_use = "resume the keys before signing requests again"]
pub(crate) struct SuspendedKeys {
    path: std::path::PathBuf,
    keys: Option<Zeroizing<Vec<u8>>>,
}

impl SuspendedKeys {
    fn take(path: std::path::PathBuf) -> Result<Self> {
        let keys = Zeroizing::new(std::fs::read(&path)?);
        std::fs::remove_file(&path)?;
        Ok(Self {
            path,
            keys: Some(keys),
        })
    }

    pub(crate) fn resume(mut self) -> Result<()> {
        self.restore()
    }

    fn restore(&mut self) -> Result<()> {
        match self.keys.take() {
            Some(keys) => super::write_bytes(&self.path, &keys, false),
            None => Ok(()),
        }
    }
}

impl Drop for SuspendedKeys {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            tracing::warn!("Could not restore the client identity's keys: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspended_keys_are_off_disk_for_child_processes_until_resumed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("network-keys").join("request.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "private keys").unwrap();
        let absent_in_child = || {
            std::process::Command::new("sh")
                .args(["-c", "test ! -e \"$1\"", "sh"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        };

        let suspended = SuspendedKeys::take(path.clone()).unwrap();
        assert!(absent_in_child());
        suspended.resume().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "private keys");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "restored keys stay private");
        }

        // An early return restores them too.
        let failed = || -> Result<()> {
            let _suspended = SuspendedKeys::take(path.clone())?;
            assert!(absent_in_child());
            Err("build failed".into())
        };
        assert!(failed().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "private keys");

        // Without an identity there is nothing to suspend.
        assert!(!is_active());
        assert_eq!(without_keys(|| 7).unwrap(), 7);
    }

    #[test]
    fn identities_and_invitations_are_told_apart_by_prefix() {
        assert!(is_identity("  tako-identity-v1-abc"));
        assert!(!is_identity("tako-invite-abc"));
        let error = store("tako-identity-v9-abc", None).unwrap_err().to_string();
        assert!(error.contains("newer Tako version"), "{error}");
        let error = store("tako-identity-v1-!!!", None).unwrap_err().to_string();
        assert!(error.contains("malformed"), "{error}");
    }
}
