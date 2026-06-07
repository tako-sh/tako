use std::path::Path;

use super::StateStoreError;

/// Load or generate a 256-bit device encryption key.
///
/// On first call, generates a random key and writes it to `path` with 0600
/// permissions. On subsequent calls, reads the existing key from disk.
pub fn load_or_create_device_key(path: &Path) -> Result<[u8; 32], StateStoreError> {
    if path.exists() {
        let key_bytes = std::fs::read(path)
            .map_err(|e| StateStoreError::Sqlite(format!("read device key: {e}")))?;
        if key_bytes.len() != 32 {
            return Err(StateStoreError::InvalidData(format!(
                "device key must be 32 bytes, got {}",
                key_bytes.len()
            )));
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&key_bytes);
        Ok(key)
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StateStoreError::Sqlite(format!("create key dir: {e}")))?;
        }
        let mut key = [0u8; 32];
        openssl::rand::rand_bytes(&mut key)
            .map_err(|e| StateStoreError::Sqlite(format!("generate device key: {e}")))?;
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| StateStoreError::Sqlite(format!("write device key: {e}")))?;
            f.write_all(&key)
                .map_err(|e| StateStoreError::Sqlite(format!("write device key: {e}")))?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(path, &key)
                .map_err(|e| StateStoreError::Sqlite(format!("write device key: {e}")))?;
        }
        Ok(key)
    }
}
