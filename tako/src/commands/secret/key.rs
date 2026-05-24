use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::output;

use super::resolve_secret_environment;

fn read_key_bundle() -> Result<String, Box<dyn std::error::Error>> {
    use std::io::IsTerminal;

    let value = if std::io::stdin().is_terminal() {
        crate::output::password_field("Exported key")?
    } else {
        let mut value = String::new();
        let bytes = std::io::stdin().read_line(&mut value)?;
        if bytes == 0 {
            return Err("No exported key provided on stdin".into());
        }
        value.trim_end_matches(['\r', '\n']).to_string()
    };

    if value.trim().is_empty() {
        return Err("Exported key cannot be empty".into());
    }

    Ok(value.trim().to_string())
}

const INVALID_PASSPHRASE_PROMPT_ERROR: &str = "Invalid passphrase";
const INVALID_PASSPHRASE_ERROR: &str = "Invalid passphrase.";

fn read_passphrase_key_for_env(
    secrets: &crate::config::SecretsStore,
    env: &str,
    key_id: &str,
) -> Result<crate::crypto::EncryptionKey, Box<dyn std::error::Error>> {
    use std::io::IsTerminal;

    let passphrase = if std::io::stdin().is_terminal() {
        crate::output::TextField::new("Passphrase")
            .password()
            .prompt_validated(|value| {
                let key = crate::crypto::derive_key_from_passphrase(value, key_id)
                    .map_err(|_| INVALID_PASSPHRASE_PROMPT_ERROR.to_string())?;
                validate_passphrase_key_for_env(secrets, env, &key)
                    .map_err(|_| INVALID_PASSPHRASE_PROMPT_ERROR.to_string())
            })?
    } else {
        let mut value = String::new();
        let bytes = std::io::stdin().read_line(&mut value)?;
        if bytes == 0 {
            return Err("No passphrase provided on stdin".into());
        }
        value.trim_end_matches(['\r', '\n']).to_string()
    };

    if passphrase.is_empty() {
        return Err("Passphrase cannot be empty.".into());
    }

    let key = crate::crypto::derive_key_from_passphrase(&passphrase, key_id)?;
    validate_passphrase_key_for_env(secrets, env, &key)?;

    Ok(key)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyImportSource {
    ExportedKey,
    Passphrase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyStorageChoice {
    LocalFile,
    #[cfg(target_os = "macos")]
    ICloudKeychain,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ExportedKeyBundle {
    version: u8,
    id: String,
    key: String,
}

pub(super) fn encode_key_bundle(key_id: &str, key: &crate::crypto::EncryptionKey) -> String {
    let bundle = ExportedKeyBundle {
        version: 1,
        id: key_id.to_string(),
        key: key.to_base64(),
    };
    let json = serde_json::to_vec(&bundle).expect("serialize key bundle");
    BASE64_URL.encode(json)
}

pub(super) fn decode_key_bundle(
    input: &str,
) -> Result<(String, crate::crypto::EncryptionKey), Box<dyn std::error::Error>> {
    let trimmed = input.trim();
    let json = BASE64_URL
        .decode(trimmed)
        .map_err(|e| format!("Invalid key payload: {e}"))?;
    let bundle: ExportedKeyBundle =
        serde_json::from_slice(&json).map_err(|e| format!("Invalid key payload: {e}"))?;
    if bundle.version != 1 {
        return Err(format!("Unsupported key version: {}", bundle.version).into());
    }
    crate::crypto::KeyStore::for_key_id(&bundle.id)?;
    let key = crate::crypto::EncryptionKey::from_base64(&bundle.key)?;
    Ok((bundle.id, key))
}

pub(super) fn resolve_key_import_source(
    passphrase: bool,
) -> Result<KeyImportSource, Box<dyn std::error::Error>> {
    match passphrase {
        true => Ok(KeyImportSource::Passphrase),
        false if output::is_interactive() => Ok(output::select(
            "Key source",
            None,
            vec![
                ("Exported key".to_string(), KeyImportSource::ExportedKey),
                ("Passphrase".to_string(), KeyImportSource::Passphrase),
            ],
        )?),
        false => Ok(KeyImportSource::ExportedKey),
    }
}

pub(super) fn environment_for_key_id(
    secrets: &crate::config::SecretsStore,
    key_id: &str,
) -> Option<String> {
    secrets
        .environment_names()
        .into_iter()
        .find(|env| secrets.get_key_id(env) == Some(key_id))
}

pub(super) async fn export_key(
    context: &crate::commands::project_context::ProjectContext,
    env: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    let key_id = secrets
        .get_key_id(env)
        .ok_or_else(|| format!("No secrets configured for environment '{}'.", env))?;
    let key_store = crate::crypto::KeyStore::for_key_id(key_id)?;
    let Some(key) = key_store.load_key_optional_with_usage_path(Some(&context.project_dir))? else {
        return Err(missing_secret_key_message(env).into());
    };
    crate::keychain::require_export_authentication()?;
    let bundle = encode_key_bundle(key_id, &key);
    copy_to_clipboard(&bundle)?;

    output::success("Key copied to clipboard.");

    Ok(())
}

pub(super) async fn import_key(
    context: &crate::commands::project_context::ProjectContext,
    passphrase: bool,
    requested_env: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let source = resolve_key_import_source(passphrase)?;
    match source {
        KeyImportSource::ExportedKey => import_exported_key(context, requested_env).await,
        KeyImportSource::Passphrase => import_passphrase_key(context, requested_env).await,
    }
}

async fn import_exported_key(
    context: &crate::commands::project_context::ProjectContext,
    requested_env: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = read_key_bundle()?;
    let (key_id, key) = decode_key_bundle(&input)?;
    let mut secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    let mut secrets_changed = false;
    let env = if let Some(env) = requested_env {
        crate::config::validate_environment_name(env)?;
        if let Some(existing_key_id) = secrets.get_key_id(env) {
            if existing_key_id != key_id {
                return Err(format!("Exported key does not match {env} key.").into());
            }
        } else {
            secrets.set_env_key_id(env, &key_id)?;
            secrets_changed = true;
        }
        Some(env.to_string())
    } else {
        environment_for_key_id(&secrets, &key_id)
    };
    if let Some(env) = &env {
        validate_imported_key_for_env("Exported key", &secrets, env, &key)?;
    }

    let key_store = crate::crypto::KeyStore::for_key_id(&key_id)?;
    save_key_with_storage_prompt(&key_store, &key, env.as_deref(), Some(&context.project_dir))?;
    if secrets_changed {
        secrets.save_to_dir(&context.project_dir)?;
    }

    if let Some(env) = env {
        output::success(&format!("Imported {} key.", output::strong(&env)));
    } else {
        output::success(&format!("Imported key {}.", output::strong(&key_id)));
    }

    Ok(())
}

async fn import_passphrase_key(
    context: &crate::commands::project_context::ProjectContext,
    requested_env: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if requested_env.is_none() && !output::is_interactive() {
        return Err("Missing required environment. Pass --env with --passphrase.".into());
    }

    let env = resolve_secret_environment(context, requested_env, "Key environment")?;
    let mut secrets = crate::config::SecretsStore::load_from_dir(&context.project_dir)?;
    let key_id = secrets.ensure_env_key_id(&env)?;
    let key = read_passphrase_key_for_env(&secrets, &env, &key_id)?;

    let key_store = crate::crypto::KeyStore::for_key_id(&key_id)?;
    save_key_with_storage_prompt(&key_store, &key, Some(&env), Some(&context.project_dir))?;
    secrets.save_to_dir(&context.project_dir)?;

    output::success(&format!("Imported {} key.", output::strong(&env)));

    Ok(())
}

fn validate_imported_key_for_env(
    source: &str,
    secrets: &crate::config::SecretsStore,
    env: &str,
    key: &crate::crypto::EncryptionKey,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(encrypted_secrets) = secrets.get_env(env) {
        for encrypted_value in encrypted_secrets.values() {
            crate::crypto::decrypt(&encrypted_value.value, key)
                .map_err(|_| format!("{source} does not decrypt {env} secrets."))?;
        }
    }
    if let Some(encrypted_credentials) = secrets.get_env_credentials(env) {
        for encrypted_value in encrypted_credentials.values() {
            crate::crypto::decrypt(&encrypted_value.value, key)
                .map_err(|_| format!("{source} does not decrypt {env} credentials."))?;
        }
    }

    Ok(())
}

pub(super) fn validate_passphrase_key_for_env(
    secrets: &crate::config::SecretsStore,
    env: &str,
    key: &crate::crypto::EncryptionKey,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_imported_key_for_env("Passphrase", secrets, env, key)
        .map_err(|_| INVALID_PASSPHRASE_ERROR.into())
}

pub(super) fn missing_secret_key_message(env: &str) -> String {
    format!(
        "Unable to decrypt {env} secrets. Run `tako secrets key import` to import an exported key or passphrase."
    )
}

fn key_store_for_env(
    env: &str,
    secrets: &crate::config::SecretsStore,
) -> Result<crate::crypto::KeyStore, Box<dyn std::error::Error>> {
    let key_id = secrets.get_key_id(env).ok_or_else(|| {
        format!(
            "No key_id found for environment '{}'. This shouldn't happen - file a bug.",
            env
        )
    })?;

    Ok(crate::crypto::KeyStore::for_key_id(key_id)?)
}

/// Load a local environment key.
pub fn load_secret_key(
    env: &str,
    secrets: &crate::config::SecretsStore,
    usage_path: Option<&Path>,
) -> Result<crate::crypto::EncryptionKey, Box<dyn std::error::Error>> {
    let key_store = key_store_for_env(env, secrets)?;
    if let Some(key) = key_store.load_key_optional_with_usage_path(usage_path)? {
        return Ok(key);
    }

    Err(missing_secret_key_message(env).into())
}

pub(crate) fn load_or_create_key_for_set(
    env: &str,
    secrets: &crate::config::SecretsStore,
    usage_path: Option<&Path>,
) -> Result<crate::crypto::EncryptionKey, Box<dyn std::error::Error>> {
    let key_store = key_store_for_env(env, secrets)?;
    match key_store.load_key_optional_with_usage_path(usage_path) {
        Ok(Some(key)) => return Ok(key),
        Ok(None) => {}
        Err(_) if secrets.env_has_encrypted_values(env) => {
            return Err(missing_secret_key_message(env).into());
        }
        Err(_) => {}
    }
    if secrets.env_has_encrypted_values(env) {
        return Err(missing_secret_key_message(env).into());
    }

    let key = crate::crypto::EncryptionKey::generate()?;
    save_key_with_storage_prompt(&key_store, &key, Some(env), usage_path)?;

    Ok(key)
}

fn save_key_with_storage_prompt(
    key_store: &crate::crypto::KeyStore,
    key: &crate::crypto::EncryptionKey,
    env: Option<&str>,
    usage_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let choice = resolve_key_storage_choice(env)?;
    save_key_with_storage_choice(key_store, key, choice, usage_path)?;

    Ok(())
}

pub(super) fn save_key_with_storage_choice(
    key_store: &crate::crypto::KeyStore,
    key: &crate::crypto::EncryptionKey,
    choice: KeyStorageChoice,
    usage_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(not(target_os = "macos"))]
    let _ = usage_path;

    match choice {
        KeyStorageChoice::LocalFile => key_store.save_key(key)?,
        #[cfg(target_os = "macos")]
        KeyStorageChoice::ICloudKeychain => {
            save_key_to_icloud_keychain(key_store, key, usage_path)?
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn save_key_to_icloud_keychain(
    key_store: &crate::crypto::KeyStore,
    key: &crate::crypto::EncryptionKey,
    usage_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let key_id = key_store
        .key_id()
        .ok_or("iCloud Keychain storage requires a key id.")?;
    crate::keychain::save_key(key_id, key, usage_path).map_err(Into::into)
}

fn resolve_key_storage_choice(
    env: Option<&str>,
) -> Result<KeyStorageChoice, Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    {
        use std::io::IsTerminal;

        if !output::is_interactive() || !std::io::stdin().is_terminal() {
            return Ok(KeyStorageChoice::LocalFile);
        }

        let hint = keychain_storage_hint(env);
        let use_icloud =
            output::confirm_with_description("Use iCloud Keychain?", Some(&hint), false)?;
        if use_icloud {
            Ok(KeyStorageChoice::ICloudKeychain)
        } else {
            Ok(KeyStorageChoice::LocalFile)
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = env;
        Ok(KeyStorageChoice::LocalFile)
    }
}

#[cfg(target_os = "macos")]
pub(super) fn keychain_storage_hint(env: Option<&str>) -> String {
    match env {
        Some(env) => format!("Syncs {env} key to your other Macs."),
        None => "Syncs this key to your other Macs.".to_string(),
    }
}

/// Ensure the encryption key for `env` is available before starting
/// long-running rendering (e.g. the deploy task tree), so missing-key errors are
/// shown against a clean terminal.
///
/// Returns immediately if the env has no encrypted values (nothing to decrypt).
pub fn ensure_secret_key_available(
    env: &str,
    secrets: &crate::config::SecretsStore,
    usage_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let has_encrypted_values = secrets.get_env(env).is_some_and(|map| !map.is_empty())
        || secrets
            .get_storage_credentials_env(env)
            .is_some_and(|map| !map.is_empty())
        || secrets
            .get_env_credentials(env)
            .is_some_and(|map| !map.is_empty());
    if !has_encrypted_values {
        return Ok(());
    }
    let _ = load_secret_key(env, secrets, usage_path)?;
    Ok(())
}

fn copy_to_clipboard(text: &str) -> Result<(), Box<dyn std::error::Error>> {
    if text.is_empty() {
        return Err("Cannot copy empty key".into());
    }

    #[cfg(target_os = "macos")]
    {
        copy_to_clipboard_command("pbcopy", &[], text)
    }

    #[cfg(target_os = "linux")]
    {
        for (cmd, args) in [
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
            ("xsel", &["--clipboard", "--input"][..]),
        ] {
            if copy_to_clipboard_command(cmd, args, text).is_ok() {
                return Ok(());
            }
        }

        Err("Failed to copy key to clipboard (tried wl-copy, xclip, xsel).".into())
    }

    #[cfg(target_os = "windows")]
    {
        copy_to_clipboard_command("clip", &[], text)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = text;
        return Err("Clipboard export is not supported on this platform".into());
    }
}

fn copy_to_clipboard_command(
    cmd: &str,
    args: &[&str],
    text: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new(cmd).args(args).stdin(Stdio::piped()).spawn()?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or("Failed to open clipboard process stdin")?;
    stdin.write_all(text.as_bytes())?;
    drop(stdin);

    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Clipboard command '{}' failed", cmd).into())
    }
}
