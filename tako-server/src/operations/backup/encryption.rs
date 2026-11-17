//! Backup keys. The file encryption itself lives in
//! [`tako_storage::encryption`], shared with the CLI.

use tako_core::BackupKeyBinding;

pub(crate) use tako_storage::encryption::{
    decrypt_file as decrypt_backup_file, encrypt_file as encrypt_backup_file,
};

pub(super) fn active_backup_key(
    backup: &tako_core::BackupBinding,
) -> Result<&BackupKeyBinding, String> {
    backup
        .backup_keys
        .last()
        .ok_or_else(|| "Backup encryption key is missing.".to_string())
}

pub(super) fn find_backup_key<'a>(
    backup: &'a tako_core::BackupBinding,
    key_id: &str,
) -> Result<&'a BackupKeyBinding, String> {
    backup
        .backup_keys
        .iter()
        .find(|key| key.id == key_id)
        .ok_or_else(|| format!("Backup encryption key not found: {key_id}"))
}

pub(super) fn validate_backup_key(key: &BackupKeyBinding) -> Result<(), String> {
    let Some(suffix) = key.id.strip_prefix("backup-key-") else {
        return Err("Backup key id must start with 'backup-key-'.".to_string());
    };
    if suffix.len() != 16 || !suffix.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("Backup key id must end with 16 hex characters.".to_string());
    }
    tako_storage::encryption::decode_key(key).map(|_| ())
}

/// A key for database digests, separate from the archive encryption key.
pub(super) fn database_digest_key(key: &BackupKeyBinding) -> Result<[u8; 32], String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"tako backup database digest v1\0");
    hasher.update(tako_storage::encryption::decode_key(key)?);
    Ok(hasher.finalize().into())
}
