use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ssh_key::{Algorithm, HashAlg, LineEnding, PrivateKey, rand_core::OsRng};

const TUNNEL_AUTH_NAMESPACE: &str = "tako-tunnel-v1";
const IDENTITY_FILE: &str = "signing_key";
const DISABLE_KEYCHAIN_ENV: &str = "TAKO_IDENTITY_DISABLE_KEYCHAIN";

pub(crate) struct TakoIdentity {
    key: PrivateKey,
}

impl TakoIdentity {
    pub(crate) fn load_or_create() -> Result<Self, String> {
        if !keychain_disabled()
            && let Some(key) = load_keychain_identity()?
        {
            return Ok(Self { key });
        }
        let path = identity_path()?;
        if path.exists() {
            return load_local_identity(&path).map(|key| Self { key });
        }

        let key = generate_identity()?;
        if !keychain_disabled() && save_keychain_identity(&key).is_ok() {
            return Ok(Self { key });
        }
        save_local_identity(&path, &key)?;
        Ok(Self { key })
    }

    pub(crate) fn public_key(&self) -> Result<String, String> {
        self.key
            .public_key()
            .to_openssh()
            .map_err(|error| format!("encode Tako Identity public key: {error}"))
    }

    pub(crate) fn sign_tunnel(
        &self,
        nonce: &str,
        app: &str,
        host: &str,
        public_key: &str,
    ) -> Result<String, String> {
        let message = tunnel_auth_message(nonce, app, host, public_key);
        let signature = self
            .key
            .sign(TUNNEL_AUTH_NAMESPACE, HashAlg::Sha512, message.as_bytes())
            .map_err(|error| format!("sign tunnel request: {error}"))?;
        let pem = signature
            .to_pem(LineEnding::LF)
            .map_err(|error| format!("encode tunnel signature: {error}"))?;
        Ok(STANDARD.encode(pem))
    }
}

fn keychain_disabled() -> bool {
    std::env::var(DISABLE_KEYCHAIN_ENV).is_ok_and(|value| value == "1")
}

fn tunnel_auth_message(nonce: &str, app: &str, host: &str, public_key: &str) -> String {
    format!(
        "{TUNNEL_AUTH_NAMESPACE}\n{}\n{}\n{}\n{}\n",
        nonce.trim(),
        app.trim(),
        host.trim(),
        public_key.trim()
    )
}

fn generate_identity() -> Result<PrivateKey, String> {
    let mut rng = OsRng;
    PrivateKey::random(&mut rng, Algorithm::Ed25519)
        .map_err(|error| format!("create Tako Identity: {error}"))
}

fn identity_path() -> Result<PathBuf, String> {
    crate::paths::tako_data_dir()
        .map(|dir| dir.join("identity").join(IDENTITY_FILE))
        .map_err(|error| format!("find Tako data directory: {error}"))
}

fn load_local_identity(path: &Path) -> Result<PrivateKey, String> {
    PrivateKey::read_openssh_file(path)
        .map_err(|error| format!("read Tako Identity from {}: {error}", path.display()))
}

fn save_local_identity(path: &Path, key: &PrivateKey) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create Tako Identity directory: {error}"))?;
    }
    key.write_openssh_file(path, LineEnding::LF)
        .map_err(|error| format!("save Tako Identity to {}: {error}", path.display()))
}

#[cfg(target_os = "macos")]
fn load_keychain_identity() -> Result<Option<PrivateKey>, String> {
    use security_framework::passwords::generic_password;

    match generic_password(keychain_options()) {
        Ok(bytes) => {
            let pem =
                String::from_utf8(bytes).map_err(|error| format!("read Tako Identity: {error}"))?;
            PrivateKey::from_openssh(pem.as_bytes())
                .map(Some)
                .map_err(|error| format!("read Tako Identity from iCloud Keychain: {error}"))
        }
        Err(error) if keychain_item_missing(&error) || keychain_unavailable(&error) => Ok(None),
        Err(error) => Err(format!("read Tako Identity from iCloud Keychain: {error}")),
    }
}

#[cfg(not(target_os = "macos"))]
fn load_keychain_identity() -> Result<Option<PrivateKey>, String> {
    Ok(None)
}

#[cfg(target_os = "macos")]
fn save_keychain_identity(key: &PrivateKey) -> Result<(), String> {
    use security_framework::passwords::set_generic_password_options;

    let pem = key
        .to_openssh(LineEnding::LF)
        .map_err(|error| format!("encode Tako Identity: {error}"))?;
    set_generic_password_options(pem.as_bytes(), keychain_options())
        .map_err(|error| format!("save Tako Identity to iCloud Keychain: {error}"))
}

#[cfg(not(target_os = "macos"))]
fn save_keychain_identity(_key: &PrivateKey) -> Result<(), String> {
    Err("iCloud Keychain is unavailable".to_string())
}

#[cfg(target_os = "macos")]
fn keychain_options() -> security_framework::passwords::PasswordOptions {
    use security_framework::passwords::PasswordOptions;

    let mut options = PasswordOptions::new_generic_password("Tako Identity", "identity");
    options.use_protected_keychain();
    options.set_access_synchronized(Some(true));
    options.set_label("Tako Identity");
    options
}

#[cfg(target_os = "macos")]
fn keychain_item_missing(error: &security_framework::base::Error) -> bool {
    error.code() == -25300
}

#[cfg(target_os = "macos")]
fn keychain_unavailable(error: &security_framework::base::Error) -> bool {
    error.code() == -25307
        || error.code() == -34018
        || error.to_string().contains("No keychain is available")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        home: Option<std::ffi::OsString>,
        keychain: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_tako_home(path: &Path) -> Self {
            let home = std::env::var_os("TAKO_HOME");
            let keychain = std::env::var_os(DISABLE_KEYCHAIN_ENV);
            unsafe {
                std::env::set_var("TAKO_HOME", path);
                std::env::set_var(DISABLE_KEYCHAIN_ENV, "1");
            }
            Self { home, keychain }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.home.take() {
                    Some(value) => std::env::set_var("TAKO_HOME", value),
                    None => std::env::remove_var("TAKO_HOME"),
                }
                match self.keychain.take() {
                    Some(value) => std::env::set_var(DISABLE_KEYCHAIN_ENV, value),
                    None => std::env::remove_var(DISABLE_KEYCHAIN_ENV),
                }
            }
        }
    }

    #[test]
    fn local_identity_is_stable() {
        let _lock = ENV_LOCK.lock().unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = EnvGuard::set_tako_home(temp.path());

        let first = TakoIdentity::load_or_create()
            .unwrap()
            .public_key()
            .unwrap();
        let second = TakoIdentity::load_or_create()
            .unwrap()
            .public_key()
            .unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn tunnel_signature_is_bound_to_public_key() {
        let _lock = ENV_LOCK.lock().unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = EnvGuard::set_tako_home(temp.path());
        let identity = TakoIdentity::load_or_create().unwrap();
        let public_key = identity.public_key().unwrap();

        let signature = identity
            .sign_tunnel("nonce", "app", "app-id.tako.website", &public_key)
            .unwrap();

        assert!(!signature.is_empty());
    }
}
