use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use openssl::symm::{Cipher, Crypter, Mode};
use tako_core::{BackupEncryptionInfo, BackupKeyBinding};

pub(super) const BACKUP_ENCRYPTION_ALGORITHM: &str = "aes-256-gcm";

const KEY_SIZE: usize = 32;
const NONCE_SIZE: usize = 12;
const TAG_SIZE: usize = 16;
const BUFFER_SIZE: usize = 64 * 1024;

pub(super) fn encrypt_backup_file(
    plaintext_path: &Path,
    encrypted_path: &Path,
    key: &BackupKeyBinding,
) -> Result<BackupEncryptionInfo, String> {
    let key_bytes = decode_backup_key(key)?;
    let mut nonce = [0_u8; NONCE_SIZE];
    openssl::rand::rand_bytes(&mut nonce)
        .map_err(|e| format!("generate backup encryption nonce: {e}"))?;

    let mut input = File::open(plaintext_path)
        .map_err(|e| format!("open backup archive {}: {e}", plaintext_path.display()))?;
    let mut output = File::create(encrypted_path).map_err(|e| {
        format!(
            "create encrypted backup archive {}: {e}",
            encrypted_path.display()
        )
    })?;

    let mut crypter = Crypter::new(
        Cipher::aes_256_gcm(),
        Mode::Encrypt,
        &key_bytes,
        Some(&nonce),
    )
    .map_err(|e| format!("create backup encryptor: {e}"))?;
    crypt_file(&mut input, &mut output, &mut crypter)?;

    let mut tag = [0_u8; TAG_SIZE];
    crypter
        .get_tag(&mut tag)
        .map_err(|e| format!("finalize backup encryption tag: {e}"))?;
    output
        .flush()
        .map_err(|e| format!("flush encrypted backup archive: {e}"))?;

    Ok(BackupEncryptionInfo {
        algorithm: BACKUP_ENCRYPTION_ALGORITHM.to_string(),
        key_id: key.id.clone(),
        nonce_base64: BASE64.encode(nonce),
        tag_base64: BASE64.encode(tag),
    })
}

pub(super) fn decrypt_backup_file(
    encrypted_path: &Path,
    plaintext_path: &Path,
    key: &BackupKeyBinding,
    encryption: &BackupEncryptionInfo,
) -> Result<(), String> {
    if encryption.algorithm != BACKUP_ENCRYPTION_ALGORITHM {
        return Err(format!(
            "Unsupported backup encryption algorithm: {}",
            encryption.algorithm
        ));
    }
    if encryption.key_id != key.id {
        return Err(format!(
            "Backup requires key {}, but {} was provided.",
            encryption.key_id, key.id
        ));
    }

    let key_bytes = decode_backup_key(key)?;
    let nonce = decode_exact_base64(&encryption.nonce_base64, NONCE_SIZE, "nonce")?;
    let tag = decode_exact_base64(&encryption.tag_base64, TAG_SIZE, "tag")?;

    let mut input = File::open(encrypted_path).map_err(|e| {
        format!(
            "open encrypted backup archive {}: {e}",
            encrypted_path.display()
        )
    })?;
    let mut output = File::create(plaintext_path).map_err(|e| {
        format!(
            "create decrypted backup archive {}: {e}",
            plaintext_path.display()
        )
    })?;

    let mut crypter = Crypter::new(
        Cipher::aes_256_gcm(),
        Mode::Decrypt,
        &key_bytes,
        Some(&nonce),
    )
    .map_err(|e| format!("create backup decryptor: {e}"))?;
    crypter
        .set_tag(&tag)
        .map_err(|e| format!("set backup encryption tag: {e}"))?;
    crypt_file(&mut input, &mut output, &mut crypter)?;
    output
        .flush()
        .map_err(|e| format!("flush decrypted backup archive: {e}"))?;
    Ok(())
}

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
    decode_backup_key(key).map(|_| ())
}

fn crypt_file(input: &mut File, output: &mut File, crypter: &mut Crypter) -> Result<(), String> {
    let mut input_buf = [0_u8; BUFFER_SIZE];
    let mut output_buf = vec![0_u8; BUFFER_SIZE + Cipher::aes_256_gcm().block_size()];
    loop {
        let read = input
            .read(&mut input_buf)
            .map_err(|e| format!("read backup archive: {e}"))?;
        if read == 0 {
            break;
        }
        let written = crypter
            .update(&input_buf[..read], &mut output_buf)
            .map_err(|e| format!("process backup archive encryption: {e}"))?;
        output
            .write_all(&output_buf[..written])
            .map_err(|e| format!("write backup archive: {e}"))?;
    }

    let written = crypter
        .finalize(&mut output_buf)
        .map_err(|e| format!("finalize backup archive encryption: {e}"))?;
    output
        .write_all(&output_buf[..written])
        .map_err(|e| format!("write backup archive final block: {e}"))?;
    Ok(())
}

fn decode_backup_key(key: &BackupKeyBinding) -> Result<[u8; KEY_SIZE], String> {
    let bytes = decode_exact_base64(&key.key_base64, KEY_SIZE, "backup key")?;
    let mut key_bytes = [0_u8; KEY_SIZE];
    key_bytes.copy_from_slice(&bytes);
    Ok(key_bytes)
}

fn decode_exact_base64(value: &str, expected_len: usize, label: &str) -> Result<Vec<u8>, String> {
    let bytes = BASE64
        .decode(value)
        .map_err(|e| format!("decode backup encryption {label}: {e}"))?;
    if bytes.len() != expected_len {
        return Err(format!(
            "Backup encryption {label} must be {expected_len} bytes, got {}.",
            bytes.len()
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

    fn test_key(id: &str) -> BackupKeyBinding {
        BackupKeyBinding {
            id: id.to_string(),
            key_base64: BASE64.encode([7_u8; KEY_SIZE]),
        }
    }

    #[test]
    fn encrypt_and_decrypt_backup_file_round_trips() {
        let temp = tempfile::TempDir::new().unwrap();
        let plain = temp.path().join("plain.tar.zst");
        let encrypted = temp.path().join("plain.tar.zst.enc");
        let decrypted = temp.path().join("decrypted.tar.zst");
        std::fs::write(&plain, b"backup archive bytes").unwrap();
        let key = test_key("backup-key-0123456789abcdef");

        let metadata = encrypt_backup_file(&plain, &encrypted, &key).unwrap();
        assert_eq!(metadata.algorithm, BACKUP_ENCRYPTION_ALGORITHM);
        assert_eq!(metadata.key_id, key.id);
        assert_ne!(std::fs::read(&encrypted).unwrap(), b"backup archive bytes");

        decrypt_backup_file(&encrypted, &decrypted, &key, &metadata).unwrap();
        assert_eq!(std::fs::read(&decrypted).unwrap(), b"backup archive bytes");
    }

    #[test]
    fn decrypt_rejects_wrong_key_id() {
        let temp = tempfile::TempDir::new().unwrap();
        let plain = temp.path().join("plain.tar.zst");
        let encrypted = temp.path().join("plain.tar.zst.enc");
        let decrypted = temp.path().join("decrypted.tar.zst");
        std::fs::write(&plain, b"backup archive bytes").unwrap();
        let key = test_key("backup-key-0123456789abcdef");
        let metadata = encrypt_backup_file(&plain, &encrypted, &key).unwrap();
        let wrong_key = test_key("backup-key-fedcba9876543210");

        let error = decrypt_backup_file(&encrypted, &decrypted, &wrong_key, &metadata).unwrap_err();

        assert!(error.contains("Backup requires key"), "{error}");
    }
}
