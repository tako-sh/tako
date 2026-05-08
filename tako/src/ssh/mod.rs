//! SSH client for remote server operations
//!
//! Provides async SSH connectivity for:
//! - Command execution
//! - File upload/download via SFTP
//! - Streaming command output

mod client;
mod error;
mod sftp;

use std::path::Path;
use std::sync::Mutex;

static KEY_PASSPHRASE: Mutex<Option<String>> = Mutex::new(None);

pub use client::*;
pub use error::*;
pub use sftp::*;

pub fn set_key_passphrase(passphrase: Option<String>) {
    *KEY_PASSPHRASE.lock().expect("SSH passphrase lock poisoned") = passphrase;
}

pub(crate) fn configured_key_passphrase() -> Option<String> {
    KEY_PASSPHRASE
        .lock()
        .expect("SSH passphrase lock poisoned")
        .clone()
}

pub(crate) fn key_passphrase_for_path(path: &Path) -> Option<String> {
    if let Some(passphrase) = configured_key_passphrase() {
        return Some(passphrase);
    }

    if !crate::output::is_interactive() {
        return None;
    }

    let passphrase =
        crate::output::TextField::new(&format!("SSH passphrase for {}", path.display()))
            .password()
            .optional()
            .prompt()
            .ok()?;
    set_key_passphrase(Some(passphrase.clone()));
    Some(passphrase)
}

pub(crate) fn default_key_needs_passphrase() -> bool {
    let Some(keys_dir) = dirs::home_dir().map(|home| home.join(".ssh")) else {
        return false;
    };

    ["id_ed25519", "id_rsa", "id_ecdsa"]
        .iter()
        .map(|name| keys_dir.join(name))
        .any(|path| {
            path.exists()
                && matches!(
                    russh::keys::load_secret_key(path, None),
                    Err(russh::keys::Error::KeyIsEncrypted)
                )
        })
}
