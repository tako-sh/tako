use openssl::symm::{Cipher, decrypt_aead, encrypt_aead};

use super::StateStoreError;

pub(super) fn encrypt_blob(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, StateStoreError> {
    let cipher = Cipher::aes_256_gcm();
    let mut nonce = [0u8; 12];
    openssl::rand::rand_bytes(&mut nonce)
        .map_err(|e| StateStoreError::Sqlite(format!("generate nonce: {e}")))?;
    let mut tag = [0u8; 16];
    let ciphertext = encrypt_aead(cipher, key, Some(&nonce), &[], plaintext, &mut tag)
        .map_err(|e| StateStoreError::Sqlite(format!("encrypt: {e}")))?;
    let mut blob = Vec::with_capacity(12 + 16 + ciphertext.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&tag);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

pub(super) fn decrypt_blob(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, StateStoreError> {
    if blob.len() < 28 {
        return Err(StateStoreError::InvalidData(
            "encrypted blob too short".to_string(),
        ));
    }
    let cipher = Cipher::aes_256_gcm();
    let nonce = &blob[..12];
    let tag = &blob[12..28];
    let ciphertext = &blob[28..];
    decrypt_aead(cipher, key, Some(nonce), &[], ciphertext, tag)
        .map_err(|e| StateStoreError::InvalidData(format!("decrypt secrets: {e}")))
}
