use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{ConfigError, Result};

/// AES-256 key size in bytes
const KEY_SIZE: usize = 32;

/// AES-GCM nonce size in bytes
const NONCE_SIZE: usize = 12;

/// Key id size in bytes (16 hex chars).
const KEY_ID_SIZE: usize = 8;

/// PBKDF2 iteration count for passphrase-derived environment keys.
const PASSPHRASE_KDF_ROUNDS: u32 = 600_000;

/// Encryption key for secrets
#[derive(Clone)]
pub struct EncryptionKey {
    key: [u8; KEY_SIZE],
}

impl EncryptionKey {
    /// Generate a new random encryption key.
    pub fn generate() -> Result<Self> {
        let mut key = [0u8; KEY_SIZE];
        getrandom::fill(&mut key)
            .map_err(|e| ConfigError::Encryption(format!("Failed to generate key: {}", e)))?;
        Ok(Self { key })
    }

    /// Create from raw bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != KEY_SIZE {
            return Err(ConfigError::Encryption(format!(
                "Key must be {} bytes, got {}",
                KEY_SIZE,
                bytes.len()
            )));
        }
        let mut key = [0u8; KEY_SIZE];
        key.copy_from_slice(bytes);
        Ok(Self { key })
    }

    /// Create from base64-encoded string
    pub fn from_base64(encoded: &str) -> Result<Self> {
        let bytes = BASE64
            .decode(encoded)
            .map_err(|e| ConfigError::Encryption(format!("Invalid base64 key: {}", e)))?;
        Self::from_bytes(&bytes)
    }

    /// Export as base64-encoded string
    pub fn to_base64(&self) -> String {
        BASE64.encode(self.key)
    }

    /// Get the raw key bytes
    pub fn as_bytes(&self) -> &[u8; KEY_SIZE] {
        &self.key
    }
}

/// Generate a random environment key id.
pub fn generate_key_id() -> String {
    let mut key_id = [0u8; KEY_ID_SIZE];
    getrandom::fill(&mut key_id).expect("operating system RNG unavailable");
    hex::encode(key_id)
}

/// Human-readable macOS Keychain label for an environment key.
pub fn keychain_label_for_key_id(key_id: &str) -> String {
    format!("Tako secrets key {key_id}")
}

/// Derive an environment key from a passphrase and environment key id.
pub fn derive_key_from_passphrase(passphrase: &str, key_id: &str) -> Result<EncryptionKey> {
    KeyStore::for_key_id(key_id)?;
    if passphrase.is_empty() {
        return Err(ConfigError::Validation(
            "Passphrase cannot be empty.".to_string(),
        ));
    }

    let salt = format!("tako-secrets-v1:{key_id}");
    let key = pbkdf2::pbkdf2_hmac_array::<sha2::Sha256, KEY_SIZE>(
        passphrase.as_bytes(),
        salt.as_bytes(),
        PASSPHRASE_KDF_ROUNDS,
    );
    EncryptionKey::from_bytes(&key)
}

/// Encrypt a plaintext string using AES-256-GCM
///
/// Returns a base64-encoded string containing: nonce (12 bytes) + ciphertext
pub fn encrypt(plaintext: &str, key: &EncryptionKey) -> Result<String> {
    let cipher = Aes256Gcm::new_from_slice(&key.key)
        .map_err(|e| ConfigError::Encryption(format!("Failed to create cipher: {}", e)))?;

    // Generate random nonce
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    getrandom::fill(&mut nonce_bytes)
        .map_err(|e| ConfigError::Encryption(format!("Failed to generate nonce: {}", e)))?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| ConfigError::Encryption(format!("Encryption failed: {}", e)))?;

    // Combine nonce + ciphertext
    let mut combined = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    Ok(BASE64.encode(combined))
}

/// Decrypt a base64-encoded ciphertext using AES-256-GCM
///
/// Expects format: base64(nonce (12 bytes) + ciphertext)
pub fn decrypt(encrypted: &str, key: &EncryptionKey) -> Result<String> {
    let combined = BASE64
        .decode(encrypted)
        .map_err(|e| ConfigError::Decryption(format!("Invalid base64: {}", e)))?;

    if combined.len() < NONCE_SIZE {
        return Err(ConfigError::Decryption("Ciphertext too short".to_string()));
    }

    let (nonce_bytes, ciphertext) = combined.split_at(NONCE_SIZE);
    let nonce = Nonce::from_slice(nonce_bytes);

    let cipher = Aes256Gcm::new_from_slice(&key.key)
        .map_err(|e| ConfigError::Decryption(format!("Failed to create cipher: {}", e)))?;

    let plaintext = cipher.decrypt(nonce, ciphertext).map_err(|_| {
        ConfigError::Decryption("Decryption failed (wrong key or corrupted data)".to_string())
    })?;

    String::from_utf8(plaintext)
        .map_err(|e| ConfigError::Decryption(format!("Invalid UTF-8: {}", e)))
}

/// Key storage manager
///
/// Stores encryption keys by environment key id.
///
/// File path: `$TAKO_HOME/keys/{key_id}`
pub struct KeyStore {
    /// Environment key id, when this store is tied to a project key.
    key_id: Option<String>,

    /// Path to the key file
    key_path: PathBuf,
}

impl KeyStore {
    /// Create a key store keyed by an exported/imported key id.
    pub fn for_key_id(key_id: &str) -> Result<Self> {
        if key_id.len() != 16 || !key_id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ConfigError::Validation(format!(
                "Invalid key id '{}'. Expected 16 hex characters.",
                key_id
            )));
        }

        let data_dir = crate::paths::tako_data_dir().map_err(|e| {
            ConfigError::Validation(format!("Could not determine tako data directory: {}", e))
        })?;

        Ok(Self {
            key_id: Some(key_id.to_string()),
            key_path: data_dir.join("keys").join(key_id),
        })
    }

    /// Create a key store with a custom path
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            key_id: None,
            key_path: path,
        }
    }

    /// Get key id when this store is tied to an environment key.
    pub fn key_id(&self) -> Option<&str> {
        self.key_id.as_deref()
    }

    /// Get key file path
    pub fn key_path(&self) -> &Path {
        &self.key_path
    }

    /// Load the encryption key from storage
    pub fn load_key(&self) -> Result<EncryptionKey> {
        if let Some(key) = self.load_key_optional()? {
            return Ok(key);
        }

        Err(ConfigError::FileRead(
            self.key_path.clone(),
            std::io::Error::new(std::io::ErrorKind::NotFound, "key not found"),
        ))
    }

    /// Load the encryption key if it exists in iCloud Keychain or local file storage.
    pub fn load_key_optional(&self) -> Result<Option<EncryptionKey>> {
        self.load_key_optional_with_usage_path(None)
    }

    /// Load the encryption key and record the project path when iCloud Keychain is used.
    pub fn load_key_optional_with_usage_path(
        &self,
        usage_path: Option<&Path>,
    ) -> Result<Option<EncryptionKey>> {
        if let Some(key_id) = self.key_id.as_deref()
            && let Some(key) = crate::keychain::load_key(key_id).map_err(ConfigError::Encryption)?
        {
            if let Some(path) = usage_path {
                crate::keychain::mark_key_used(key_id, path).map_err(ConfigError::Encryption)?;
            }
            return Ok(Some(key));
        }
        if self.key_path.exists() {
            return Ok(Some(self.load_file_key()?));
        }

        Ok(None)
    }

    fn load_file_key(&self) -> Result<EncryptionKey> {
        let encoded = fs::read_to_string(&self.key_path)
            .map_err(|e| ConfigError::FileRead(self.key_path.clone(), e))?;
        EncryptionKey::from_base64(encoded.trim())
    }

    /// Save the encryption key to storage
    pub fn save_key(&self, key: &EncryptionKey) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.key_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ConfigError::FileWrite(parent.to_path_buf(), e))?;
        }

        // Write key with restrictive permissions.
        // On Unix, create with 0600 from the start to avoid a window where the
        // key is world-readable (TOCTOU between write and chmod).
        let encoded = key.to_base64();
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.key_path)
                .map_err(|e| ConfigError::FileWrite(self.key_path.clone(), e))?;
            f.write_all(encoded.as_bytes())
                .map_err(|e| ConfigError::FileWrite(self.key_path.clone(), e))?;
        }
        #[cfg(not(unix))]
        {
            fs::write(&self.key_path, &encoded)
                .map_err(|e| ConfigError::FileWrite(self.key_path.clone(), e))?;
        }

        Ok(())
    }

    /// Check if a key exists
    pub fn key_exists(&self) -> bool {
        self.load_key_optional().is_ok_and(|key| key.is_some())
    }

    /// Delete the key
    pub fn delete_key(&self) -> Result<()> {
        if self.key_path.exists() {
            fs::remove_file(&self.key_path)
                .map_err(|e| ConfigError::FileWrite(self.key_path.clone(), e))?;
        }
        if let Some(key_id) = self.key_id.as_deref() {
            crate::keychain::delete_key(key_id).map_err(ConfigError::Encryption)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use tempfile::TempDir;

    fn with_temp_tako_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _lock = crate::paths::test_tako_home_env_lock();

        let temp = TempDir::new().unwrap();
        let previous = std::env::var_os("TAKO_HOME");
        unsafe {
            std::env::set_var("TAKO_HOME", temp.path());
        }

        struct ResetEnv(Option<OsString>);
        impl Drop for ResetEnv {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("TAKO_HOME", value) },
                    None => unsafe { std::env::remove_var("TAKO_HOME") },
                }
            }
        }
        let _reset = ResetEnv(previous);

        f(temp.path())
    }

    // ==================== Key Generation Tests ====================

    #[test]
    fn test_generate_key_produces_correct_length_key() {
        let key = EncryptionKey::generate().unwrap();
        assert_eq!(key.as_bytes().len(), KEY_SIZE);
    }

    #[test]
    fn test_generate_key_is_random() {
        let key1 = EncryptionKey::generate().unwrap();
        let key2 = EncryptionKey::generate().unwrap();
        assert_ne!(key1.as_bytes(), key2.as_bytes());
    }

    #[test]
    fn test_generated_key_encrypt_decrypt_round_trip() {
        let key = EncryptionKey::generate().unwrap();
        let plaintext = "Hello, World!";

        let encrypted = encrypt(plaintext, &key).unwrap();
        let decrypted = decrypt(&encrypted, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    // ==================== Key id Tests ====================

    #[test]
    fn test_generate_key_id_is_random() {
        let key_id1 = generate_key_id();
        let key_id2 = generate_key_id();
        assert_ne!(key_id1, key_id2);
    }

    #[test]
    fn test_generate_key_id_produces_hex_id() {
        let key_id = generate_key_id();
        assert_eq!(key_id.len(), 16);
        assert!(key_id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ==================== Encryption Tests ====================

    #[test]
    fn test_key_from_bytes() {
        let bytes = [0u8; KEY_SIZE];
        let key = EncryptionKey::from_bytes(&bytes).unwrap();
        assert_eq!(key.as_bytes(), &bytes);
    }

    #[test]
    fn test_key_from_bytes_wrong_size() {
        let bytes = [0u8; 16];
        let result = EncryptionKey::from_bytes(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_key_base64_round_trip() {
        let key = EncryptionKey::generate().unwrap();
        let encoded = key.to_base64();
        let decoded = EncryptionKey::from_base64(&encoded).unwrap();
        assert_eq!(key.as_bytes(), decoded.as_bytes());
    }

    #[test]
    fn test_encrypt_decrypt_round_trip() {
        let key = EncryptionKey::generate().unwrap();
        let plaintext = "Hello, World!";

        let encrypted = encrypt(plaintext, &key).unwrap();
        let decrypted = decrypt(&encrypted, &key).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_produces_different_ciphertext() {
        let key = EncryptionKey::generate().unwrap();
        let plaintext = "Hello, World!";

        let encrypted1 = encrypt(plaintext, &key).unwrap();
        let encrypted2 = encrypt(plaintext, &key).unwrap();

        // Different ciphertexts due to random nonce
        assert_ne!(encrypted1, encrypted2);

        // But both decrypt to the same plaintext
        assert_eq!(decrypt(&encrypted1, &key).unwrap(), plaintext);
        assert_eq!(decrypt(&encrypted2, &key).unwrap(), plaintext);
    }

    #[test]
    fn test_decrypt_with_wrong_key_fails() {
        let key1 = EncryptionKey::generate().unwrap();
        let key2 = EncryptionKey::generate().unwrap();
        let plaintext = "Hello, World!";

        let encrypted = encrypt(plaintext, &key1).unwrap();
        let result = decrypt(&encrypted, &key2);

        assert!(result.is_err());
    }

    #[test]
    fn test_encrypt_unicode() {
        let key = EncryptionKey::generate().unwrap();
        let plaintext = "Hello, 世界! 🔐";

        let encrypted = encrypt(plaintext, &key).unwrap();
        let decrypted = decrypt(&encrypted, &key).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    // ==================== KeyStore Tests ====================

    #[test]
    fn test_key_store_for_key_id_uses_id_as_file_name() {
        with_temp_tako_home(|temp_home| {
            let key_id = generate_key_id();
            let store = KeyStore::for_key_id(&key_id).unwrap();

            // Path should be under keys/ with the key id as the file name.
            let key_path = store.key_path();
            assert!(key_path.starts_with(temp_home.join("keys")));
            let file_name = key_path.file_name().unwrap().to_str().unwrap();
            assert_eq!(file_name, key_id);
            assert!(
                file_name.chars().all(|c| c.is_ascii_hexdigit()),
                "file name should be hex: {}",
                file_name
            );
        });
    }

    #[test]
    fn test_key_store_for_key_id_uses_direct_id() {
        with_temp_tako_home(|temp_home| {
            let key_id = generate_key_id();
            let store = KeyStore::for_key_id(&key_id).unwrap();

            assert_eq!(store.key_path(), temp_home.join("keys").join(&key_id));
        });
    }

    #[test]
    fn test_key_store_for_key_id_rejects_invalid_ids() {
        with_temp_tako_home(|_| {
            assert!(KeyStore::for_key_id("short").is_err());
            assert!(KeyStore::for_key_id("zzzzzzzzzzzzzzzz").is_err());
        });
    }

    #[test]
    fn test_key_store_for_key_id_is_deterministic() {
        with_temp_tako_home(|_| {
            let key_id = generate_key_id();
            let store1 = KeyStore::for_key_id(&key_id).unwrap();
            let store2 = KeyStore::for_key_id(&key_id).unwrap();
            assert_eq!(store1.key_path(), store2.key_path());
        });
    }

    #[test]
    fn test_keychain_label_names_secrets_key() {
        assert_eq!(
            keychain_label_for_key_id("0123456789abcdef"),
            "Tako secrets key 0123456789abcdef"
        );
    }

    #[test]
    fn test_derive_key_from_passphrase_is_deterministic() {
        let key_a =
            derive_key_from_passphrase("correct horse battery staple", "0123456789abcdef").unwrap();
        let key_b =
            derive_key_from_passphrase("correct horse battery staple", "0123456789abcdef").unwrap();

        assert_eq!(key_a.as_bytes(), key_b.as_bytes());
    }

    #[test]
    fn test_derive_key_from_passphrase_is_scoped_by_key_id() {
        let key_a =
            derive_key_from_passphrase("correct horse battery staple", "0123456789abcdef").unwrap();
        let key_b =
            derive_key_from_passphrase("correct horse battery staple", "fedcba9876543210").unwrap();

        assert_ne!(key_a.as_bytes(), key_b.as_bytes());
    }

    #[test]
    fn test_key_store_different_key_ids_different_paths() {
        with_temp_tako_home(|_| {
            let key_id_a = generate_key_id();
            let key_id_b = generate_key_id();
            let store_a = KeyStore::for_key_id(&key_id_a).unwrap();
            let store_b = KeyStore::for_key_id(&key_id_b).unwrap();
            assert_ne!(store_a.key_path(), store_b.key_path());
        });
    }

    #[test]
    fn test_key_store_save_and_load() {
        with_temp_tako_home(|_| {
            let key_id = generate_key_id();
            let store = KeyStore::for_key_id(&key_id).unwrap();
            let key = EncryptionKey::generate().unwrap();

            store.save_key(&key).unwrap();
            assert!(store.key_exists());

            let loaded = store.load_key().unwrap();
            assert_eq!(key.as_bytes(), loaded.as_bytes());
        });
    }

    #[test]
    fn test_key_store_delete() {
        with_temp_tako_home(|_| {
            let key_id = generate_key_id();
            let store = KeyStore::for_key_id(&key_id).unwrap();
            let key = EncryptionKey::generate().unwrap();

            store.save_key(&key).unwrap();
            assert!(store.key_exists());

            store.delete_key().unwrap();
            assert!(!store.key_exists());
        });
    }

    #[test]
    fn test_separate_key_ids_have_separate_keys() {
        with_temp_tako_home(|_| {
            let key_id_a = generate_key_id();
            let key_id_b = generate_key_id();
            let store_a = KeyStore::for_key_id(&key_id_a).unwrap();
            let store_b = KeyStore::for_key_id(&key_id_b).unwrap();

            let key_a = EncryptionKey::generate().unwrap();
            let key_b = EncryptionKey::generate().unwrap();

            store_a.save_key(&key_a).unwrap();
            store_b.save_key(&key_b).unwrap();

            let loaded_a = store_a.load_key().unwrap();
            let loaded_b = store_b.load_key().unwrap();
            assert_ne!(loaded_a.as_bytes(), loaded_b.as_bytes());
        });
    }

    /// One-off helper to generate real encrypted secrets.json for Go examples.
    /// Run with: cargo test -p tako -- generate_go_example_secrets --ignored --nocapture
    #[test]
    #[ignore]
    fn generate_go_example_secrets() {
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let example_dirs = [
            (
                repo_root.join("examples/go/basic"),
                [
                    ("development", "b451c0de00000001"),
                    ("production", "b451c0de00000002"),
                ],
            ),
            (
                repo_root.join("examples/go/gin"),
                [
                    ("development", "916c0de100000001"),
                    ("production", "916c0de100000002"),
                ],
            ),
            (
                repo_root.join("examples/go/echo"),
                [
                    ("development", "ec0c0de100000001"),
                    ("production", "ec0c0de100000002"),
                ],
            ),
            (
                repo_root.join("examples/go/chi"),
                [
                    ("development", "c810c0de00000001"),
                    ("production", "c810c0de00000002"),
                ],
            ),
        ];
        let secrets_data = [
            ("API_KEY", "sk-example-key-12345"),
            ("DATABASE_URL", "postgres://localhost:5432/myapp"),
            ("EXAMPLE_SECRET", "hello-from-tako"),
        ];

        for (dir, envs) in &example_dirs {
            let dir = dir.as_path();
            if !dir.exists() {
                eprintln!("skipping {} (not found)", dir.display());
                continue;
            }
            let tako_dir = dir.join(".tako");
            std::fs::create_dir_all(&tako_dir).unwrap();

            let mut environments = serde_json::Map::new();
            for (env_name, key_id) in envs {
                let key = derive_key_from_passphrase("tako-example", key_id).unwrap();

                let mut secrets_map = serde_json::Map::new();
                for (name, value) in &secrets_data {
                    let encrypted = encrypt(value, &key).unwrap();
                    secrets_map.insert(name.to_string(), serde_json::Value::String(encrypted));
                }

                environments.insert(
                    env_name.to_string(),
                    serde_json::json!({
                        "key_id": key_id,
                        "secrets": secrets_map
                    }),
                );
            }

            let path = tako_dir.join("secrets.json");
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&serde_json::Value::Object(environments)).unwrap(),
            )
            .unwrap();
            eprintln!("wrote {}", path.display());
        }
    }
}
