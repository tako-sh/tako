use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use russh::keys::agent::client::AgentClient;
use russh::keys::ssh_key::{self, SshSig};
use russh::keys::{Algorithm, Error as KeyError, HashAlg, PrivateKey, PublicKey, load_secret_key};

use super::ManagementError;

pub(crate) const HEADER_KEY_FINGERPRINT: &str = "x-tako-key-fingerprint";
pub(crate) const HEADER_TIMESTAMP: &str = "x-tako-timestamp";
pub(crate) const HEADER_NONCE: &str = "x-tako-nonce";
pub(crate) const HEADER_SIGNATURE: &str = "x-tako-signature";

#[derive(Debug, Clone)]
pub(crate) struct SignedHeaders {
    pub(crate) key_fingerprint: String,
    pub(crate) timestamp: String,
    pub(crate) nonce: String,
    pub(crate) signature: String,
}

pub(crate) struct ManagementSigner {
    private_keys: Vec<PrivateKey>,
    agent_keys: Vec<PublicKey>,
}

impl ManagementSigner {
    pub(crate) async fn load() -> Result<Self, ManagementError> {
        let private_keys = load_private_keys();
        let agent_keys = load_agent_keys().await;

        if private_keys.is_empty() && agent_keys.is_empty() {
            return Err(ManagementError::Message(
                "No SSH keys available for remote management auth".to_string(),
            ));
        }

        Ok(Self {
            private_keys,
            agent_keys,
        })
    }

    pub(crate) async fn sign_headers(
        &self,
        body: &[u8],
    ) -> Result<Vec<SignedHeaders>, ManagementError> {
        let mut signed = Vec::new();

        for key in &self.private_keys {
            signed.push(sign_with_private_key(key, body)?);
        }

        for public_key in &self.agent_keys {
            if let Ok(headers) = sign_with_agent(public_key, body).await {
                signed.push(headers);
            }
        }

        if signed.is_empty() {
            return Err(ManagementError::Message(
                "No SSH keys could sign remote management request".to_string(),
            ));
        }

        Ok(signed)
    }
}

fn load_private_keys() -> Vec<PrivateKey> {
    let Some(keys_dir) = dirs::home_dir().map(|home| home.join(".ssh")) else {
        return Vec::new();
    };
    ["id_ed25519", "id_rsa", "id_ecdsa"]
        .iter()
        .filter_map(|name| load_private_key(&keys_dir.join(name)).ok())
        .collect()
}

fn load_private_key(path: &Path) -> Result<PrivateKey, ManagementError> {
    if !path.exists() {
        return Err(ManagementError::Message("key not found".to_string()));
    }

    match load_secret_key(path, None) {
        Ok(key) => Ok(key),
        Err(KeyError::KeyIsEncrypted) => {
            let Some(passphrase) = crate::ssh::key_passphrase_for_path(path) else {
                return Err(ManagementError::Message(
                    KeyError::KeyIsEncrypted.to_string(),
                ));
            };
            load_secret_key(path, Some(&passphrase))
                .map_err(|error| ManagementError::Message(error.to_string()))
        }
        Err(error) => Err(ManagementError::Message(error.to_string())),
    }
}

async fn load_agent_keys() -> Vec<PublicKey> {
    let Ok(mut agent) = AgentClient::connect_env().await else {
        return Vec::new();
    };
    let Ok(identities) = agent.request_identities().await else {
        return Vec::new();
    };

    identities
        .into_iter()
        .map(|identity| identity.public_key().into_owned())
        .collect()
}

fn sign_with_private_key(key: &PrivateKey, body: &[u8]) -> Result<SignedHeaders, ManagementError> {
    let timestamp = current_timestamp()?;
    let nonce = random_nonce()?;
    let message = tako_core::management_auth_message(&timestamp, &nonce, body);
    let signature = key
        .sign(
            tako_core::MANAGEMENT_AUTH_NAMESPACE,
            HashAlg::Sha512,
            &message,
        )
        .map_err(|error| ManagementError::Message(error.to_string()))?;
    signed_headers(
        key.fingerprint(HashAlg::Sha256).to_string(),
        timestamp,
        nonce,
        signature,
    )
}

async fn sign_with_agent(
    public_key: &PublicKey,
    body: &[u8],
) -> Result<SignedHeaders, ManagementError> {
    let timestamp = current_timestamp()?;
    let nonce = random_nonce()?;
    let message = tako_core::management_auth_message(&timestamp, &nonce, body);
    let signed_data = SshSig::signed_data(
        tako_core::MANAGEMENT_AUTH_NAMESPACE,
        HashAlg::Sha512,
        &message,
    )
    .map_err(|error| ManagementError::Message(error.to_string()))?;
    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|error| ManagementError::Message(error.to_string()))?;
    let hash_alg = if matches!(public_key.algorithm(), Algorithm::Rsa { .. }) {
        Some(HashAlg::Sha512)
    } else {
        None
    };
    let signature = agent
        .sign_request_signature(public_key, hash_alg, &signed_data)
        .await
        .map_err(|error| ManagementError::Message(error.to_string()))?;
    let signature = SshSig::new(
        public_key.key_data().clone(),
        tako_core::MANAGEMENT_AUTH_NAMESPACE,
        HashAlg::Sha512,
        signature,
    )
    .map_err(|error| ManagementError::Message(error.to_string()))?;

    signed_headers(
        public_key.fingerprint(HashAlg::Sha256).to_string(),
        timestamp,
        nonce,
        signature,
    )
}

fn signed_headers(
    key_fingerprint: String,
    timestamp: String,
    nonce: String,
    signature: SshSig,
) -> Result<SignedHeaders, ManagementError> {
    let signature_pem = signature
        .to_pem(ssh_key::LineEnding::LF)
        .map_err(|error| ManagementError::Message(error.to_string()))?;
    let signature = base64::engine::general_purpose::STANDARD.encode(signature_pem);

    Ok(SignedHeaders {
        key_fingerprint,
        timestamp,
        nonce,
        signature,
    })
}

#[cfg(test)]
mod passphrase_tests {
    use super::*;

    const ENCRYPTED_ED25519_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABCRv2KPnI\n\
IRphE01i7dWiijAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIBS7MYzXocRVMCqK\n\
uxD+2gS1Q9ZtX7zYh74IFWEKRZ4OAAAAkEa8z/fYTNnkt7g2yLcFM8IQFw67+aUeTzC6V2\n\
g+KleH6OSa4Q3cbBSMhWFkNY/IjTKNNg7P2XszrFMJblBkWokMvKgh3oGfJV4Axh3RZUsS\n\
ep5Su4gT/9WhaF3n32sxVB3BhK8IDBQBfsXh+YLhP0bZFdN+jLffuAQlINtoFYY8/4vvsn\n\
l4QMs5cmnWfrM0GQ==\n\
-----END OPENSSH PRIVATE KEY-----\n";

    struct PassphraseGuard;

    impl Drop for PassphraseGuard {
        fn drop(&mut self) {
            crate::ssh::set_key_passphrase(None);
        }
    }

    #[test]
    fn load_private_key_uses_configured_passphrase() {
        let _guard = PassphraseGuard;
        let temp = tempfile::TempDir::new().expect("temp dir");
        let key_path = temp.path().join("id_ed25519");
        std::fs::write(&key_path, ENCRYPTED_ED25519_KEY).expect("write key");
        crate::ssh::set_key_passphrase(Some("testpass".to_string()));

        let key = load_private_key(&key_path).expect("load encrypted key");

        assert_eq!(key.algorithm(), Algorithm::Ed25519);
    }
}

fn current_timestamp() -> Result<String, ManagementError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| ManagementError::Message(error.to_string()))?
        .as_secs();
    Ok(timestamp.to_string())
}

fn random_nonce() -> Result<String, ManagementError> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|error| ManagementError::Message(error.to_string()))?;
    Ok(hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> PrivateKey {
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("test key")
    }

    #[test]
    fn private_key_headers_verify_against_management_auth_message() {
        let key = test_key();
        let body = br#"{"command":"list"}"#;

        let headers = sign_with_private_key(&key, body).expect("signed headers");

        let signature_pem = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(headers.signature)
                .expect("base64 signature"),
        )
        .expect("signature utf8");
        let signature = signature_pem.parse::<SshSig>().expect("ssh signature");
        let message = tako_core::management_auth_message(&headers.timestamp, &headers.nonce, body);

        key.public_key()
            .verify(tako_core::MANAGEMENT_AUTH_NAMESPACE, &message, &signature)
            .expect("signature verifies");
    }

    #[test]
    fn random_nonce_is_header_safe() {
        let nonce = random_nonce().expect("nonce");

        assert_eq!(nonce.len(), 48);
        assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
