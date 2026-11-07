//! SSH client for remote server operations
//!
//! Provides async SSH connectivity for command execution and streaming
//! command output. Release artifacts travel over the signed management
//! HTTP API, not SSH.

mod client;
mod error;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

static KEY_PASSPHRASE: Mutex<Option<String>> = Mutex::new(None);

pub use client::*;
pub use error::*;

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

// id_dsa is obsolete (OpenSSH dropped support in 7.0) — don't try it.
pub(crate) const DEFAULT_KEY_NAMES: [&str; 3] = ["id_ed25519", "id_rsa", "id_ecdsa"];

/// Expand a leading `~` to the user's home directory.
pub(crate) fn expand_tilde(path: &Path) -> PathBuf {
    let Some(home) = dirs::home_dir() else {
        return path.to_path_buf();
    };
    if path == Path::new("~") {
        return home;
    }
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}
