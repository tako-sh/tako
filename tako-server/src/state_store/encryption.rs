use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};

use super::StateStoreError;

pub(super) fn encrypt_blob(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, StateStoreError> {
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce)
        .map_err(|e| StateStoreError::Sqlite(format!("generate nonce: {e}")))?;
    let encrypted = Aes256Gcm::new(key.into())
        .encrypt(&Nonce::from(nonce), plaintext)
        .map_err(|e| StateStoreError::Sqlite(format!("encrypt: {e}")))?;
    // Keep the stored nonce || tag || ciphertext layout used by existing databases.
    let (ciphertext, tag) = encrypted.split_at(encrypted.len() - 16);
    let mut blob = Vec::with_capacity(12 + 16 + ciphertext.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(tag);
    blob.extend_from_slice(ciphertext);
    Ok(blob)
}

pub(super) fn decrypt_blob(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, StateStoreError> {
    if blob.len() < 28 {
        return Err(StateStoreError::InvalidData(
            "encrypted blob too short".to_string(),
        ));
    }
    let nonce = Nonce::try_from(&blob[..12]).expect("validated nonce length");
    let tag = &blob[12..28];
    let mut encrypted = blob[28..].to_vec();
    encrypted.extend_from_slice(tag);
    Aes256Gcm::new(key.into())
        .decrypt(&nonce, encrypted.as_slice())
        .map_err(|e| StateStoreError::InvalidData(format!("decrypt secrets: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::symm::{Cipher, decrypt_aead, encrypt_aead};

    #[test]
    fn existing_and_new_state_blobs_interoperate() {
        let key = [0x42; 32];
        for plaintext in [Vec::new(), b"stored secret".to_vec(), vec![0xff; 65537]] {
            let nonce = [0x19; 12];
            let mut tag = [0; 16];
            let encrypted = encrypt_aead(
                Cipher::aes_256_gcm(),
                &key,
                Some(&nonce),
                &[],
                &plaintext,
                &mut tag,
            )
            .unwrap();
            let legacy = [nonce.as_slice(), tag.as_slice(), &encrypted].concat();
            assert_eq!(decrypt_blob(&key, &legacy).unwrap(), plaintext);

            let blob = encrypt_blob(&key, &plaintext).unwrap();
            assert_eq!(
                decrypt_aead(
                    Cipher::aes_256_gcm(),
                    &key,
                    Some(&blob[..12]),
                    &[],
                    &blob[28..],
                    &blob[12..28]
                )
                .unwrap(),
                plaintext
            );
            assert!(decrypt_blob(&[0x43; 32], &blob).is_err());
            for index in [0, 12, blob.len() - 1] {
                let mut corrupt = blob.clone();
                corrupt[index] ^= 1;
                assert!(decrypt_blob(&key, &corrupt).is_err());
            }
        }
        for len in 0..28 {
            assert!(decrypt_blob(&key, &vec![0; len]).is_err());
        }
    }
}
