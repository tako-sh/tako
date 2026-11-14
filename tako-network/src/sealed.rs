//! X25519 / HKDF-SHA256 / AES-256-GCM envelopes for network secret keys.
//! This is separate from SSH and WireGuard, with a fixed domain and caller AAD.
use base64::{Engine, engine::general_purpose::STANDARD};
use openssl::{
    derive::Deriver,
    md::Md,
    pkey::{Id, PKey, Private},
    pkey_ctx::PkeyCtx,
    rand::rand_bytes,
    symm::{Cipher, decrypt_aead, encrypt_aead},
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{EncryptionPublicKey, LocalDeviceKeys, Result, error::invalid};

pub const MAX_SECRET_BYTES: usize = 32 * 1024;
const DOMAIN: &[u8] = b"tako-network-secret-envelope-v1\0";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ciphertext {
    pub nonce: String,
    pub value: String,
    pub tag: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedValue {
    pub ephemeral: EncryptionPublicKey,
    pub ciphertext: Ciphertext,
}

fn decode(value: &str, max: usize) -> Result<Vec<u8>> {
    if value.len() > max.div_ceil(3) * 4 {
        return Err(invalid("encrypted value", "too large"));
    }
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| invalid("encrypted value", "invalid base64"))?;
    if bytes.len() > max || STANDARD.encode(&bytes) != value {
        return Err(invalid("encrypted value", "noncanonical encoding"));
    }
    Ok(bytes)
}

pub fn random_content_key() -> Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0; 32]);
    rand_bytes(key.as_mut())?;
    Ok(key)
}

impl Ciphertext {
    pub fn encrypt(key: &[u8; 32], plaintext: &[u8], context: &[u8]) -> Result<Self> {
        if plaintext.len() > MAX_SECRET_BYTES || context.len() > 4096 {
            return Err(invalid("encrypted value", "too large"));
        }
        let mut nonce = [0; 12];
        rand_bytes(&mut nonce)?;
        let mut tag = [0; 16];
        let value = encrypt_aead(
            Cipher::aes_256_gcm(),
            key,
            Some(&nonce),
            context,
            plaintext,
            &mut tag,
        )?;
        Ok(Self {
            nonce: STANDARD.encode(nonce),
            value: STANDARD.encode(value),
            tag: STANDARD.encode(tag),
        })
    }

    pub fn validate(&self) -> Result<()> {
        if decode(&self.nonce, 12)?.len() != 12 || decode(&self.tag, 16)?.len() != 16 {
            return Err(invalid("encrypted value", "invalid nonce or tag"));
        }
        decode(&self.value, MAX_SECRET_BYTES)?;
        Ok(())
    }

    pub fn decrypt(&self, key: &[u8; 32], context: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        self.validate()?;
        if context.len() > 4096 {
            return Err(invalid("encrypted value", "context too large"));
        }
        decrypt_aead(
            Cipher::aes_256_gcm(),
            key,
            Some(&decode(&self.nonce, 12)?),
            context,
            &decode(&self.value, MAX_SECRET_BYTES)?,
            &decode(&self.tag, 16)?,
        )
        .map(Zeroizing::new)
        .map_err(|_| invalid("encrypted value", "authentication failed"))
    }
}

fn derive_key(
    private: &PKey<Private>,
    peer: &EncryptionPublicKey,
    ephemeral: &EncryptionPublicKey,
    recipient: &EncryptionPublicKey,
    context: &[u8],
) -> Result<Zeroizing<[u8; 32]>> {
    if context.len() > 4096 {
        return Err(invalid("secret envelope", "context too large"));
    }
    let public = PKey::public_key_from_raw_bytes(peer.as_bytes(), Id::X25519)?;
    let mut derive = Deriver::new(private)?;
    derive.set_peer(&public)?;
    let shared = Zeroizing::new(derive.derive_to_vec()?);
    if shared.iter().all(|byte| *byte == 0) {
        return Err(invalid("secret envelope", "invalid shared key"));
    }
    let mut info = DOMAIN.to_vec();
    info.extend_from_slice(ephemeral.as_bytes());
    info.extend_from_slice(recipient.as_bytes());
    info.extend_from_slice(&(context.len() as u64).to_be_bytes());
    info.extend_from_slice(context);
    let mut kdf = PkeyCtx::new_id(Id::HKDF)?;
    kdf.derive_init()?;
    kdf.set_hkdf_md(Md::sha256())?;
    kdf.set_hkdf_salt(DOMAIN)?;
    kdf.set_hkdf_key(&shared)?;
    kdf.add_hkdf_info(&info)?;
    let mut key = Zeroizing::new([0; 32]);
    kdf.derive(Some(key.as_mut()))?;
    Ok(key)
}

impl SealedValue {
    pub fn seal(recipient: &EncryptionPublicKey, plaintext: &[u8], context: &[u8]) -> Result<Self> {
        let ephemeral = PKey::generate_x25519()?;
        let public = EncryptionPublicKey::from_bytes(
            ephemeral
                .raw_public_key()?
                .try_into()
                .map_err(|_| invalid("secret envelope", "invalid ephemeral key"))?,
        )?;
        let key = derive_key(&ephemeral, recipient, &public, recipient, context)?;
        Ok(Self {
            ephemeral: public,
            ciphertext: Ciphertext::encrypt(&key, plaintext, context)?,
        })
    }

    pub fn open(&self, keys: &LocalDeviceKeys, context: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let recipient = keys.encryption_public_key()?;
        let key = derive_key(
            &keys.encryption,
            &self.ephemeral,
            &self.ephemeral,
            &recipient,
            context,
        )?;
        self.ciphertext.decrypt(&key, context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_binds_recipient_context_and_every_ciphertext_component() {
        let keys = LocalDeviceKeys::generate().unwrap();
        let other = LocalDeviceKeys::generate().unwrap();
        let context = b"network/app/production/API_KEY/revision";
        let sealed = SealedValue::seal(
            &keys.encryption_public_key().unwrap(),
            b"private value",
            context,
        )
        .unwrap();
        assert_eq!(&*sealed.open(&keys, context).unwrap(), b"private value");
        assert!(sealed.open(&other, context).is_err());
        assert!(
            sealed
                .open(&keys, b"network/app/staging/API_KEY/revision")
                .is_err()
        );
        for component in 0..4 {
            let mut corrupt = sealed.clone();
            match component {
                0 => corrupt.ephemeral = other.encryption_public_key().unwrap(),
                1 => corrupt.ciphertext.nonce = STANDARD.encode([1; 12]),
                2 => corrupt.ciphertext.tag = STANDARD.encode([1; 16]),
                _ => corrupt.ciphertext.value = STANDARD.encode(b"corrupt value"),
            }
            assert!(corrupt.open(&keys, context).is_err());
        }
    }

    #[test]
    fn envelopes_are_randomized_bounded_and_survive_key_persistence() {
        let keys = LocalDeviceKeys::generate().unwrap();
        let public = keys.encryption_public_key().unwrap();
        assert_ne!(public.as_bytes(), keys.wg_public_key().unwrap().as_bytes());
        let first = SealedValue::seal(&public, b"value", b"context").unwrap();
        let second = SealedValue::seal(&public, b"value", b"context").unwrap();
        assert_ne!(first, second);
        let directory = crate::private_tempdir();
        let path = directory.path().join("identity.json");
        keys.save_new(&path).unwrap();
        let loaded = LocalDeviceKeys::load(&path).unwrap();
        assert_eq!(loaded.encryption_public_key().unwrap(), public);
        assert_eq!(&*first.open(&loaded, b"context").unwrap(), b"value");
        assert!(SealedValue::seal(&public, &vec![0; MAX_SECRET_BYTES + 1], b"context").is_err());
        let mut corrupt = first;
        corrupt.ciphertext.nonce.push('\n');
        assert!(corrupt.open(&loaded, b"context").is_err());
    }
}
