use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use hyper::HeaderMap;
use parking_lot::Mutex;
use ssh_key::{HashAlg, PublicKey, SshSig};
use thiserror::Error;

pub(crate) const MANAGEMENT_AUTHORIZED_KEYS_FILE: &str = "management-authorized-keys";
pub(crate) const HEADER_KEY_FINGERPRINT: &str = "x-tako-key-fingerprint";
pub(crate) const HEADER_TIMESTAMP: &str = "x-tako-timestamp";
pub(crate) const HEADER_NONCE: &str = "x-tako-nonce";
pub(crate) const HEADER_SIGNATURE: &str = "x-tako-signature";

const AUTH_WINDOW_SECS: i64 = 300;
const MAX_SEEN_NONCES: usize = 4096;

#[derive(Debug, Error)]
pub(crate) enum ManagementAuthError {
    #[error("management auth required")]
    Required,

    #[error("management auth failed")]
    Failed,
}

#[derive(Default)]
pub(crate) struct ManagementAuthState {
    seen: Mutex<SeenNonces>,
}

#[derive(Default)]
struct SeenNonces {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl ManagementAuthState {
    fn accept_nonce(&self, nonce: &str) -> Result<(), ManagementAuthError> {
        let mut seen = self.seen.lock();
        if seen.set.contains(nonce) {
            return Err(ManagementAuthError::Failed);
        }

        seen.order.push_back(nonce.to_string());
        seen.set.insert(nonce.to_string());

        while seen.order.len() > MAX_SEEN_NONCES {
            if let Some(old) = seen.order.pop_front() {
                seen.set.remove(&old);
            }
        }

        Ok(())
    }
}

pub(crate) fn verify_signed_request(
    data_dir: &Path,
    state: &ManagementAuthState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), ManagementAuthError> {
    let key_fingerprint = required_header(headers, HEADER_KEY_FINGERPRINT)?;
    let timestamp = required_header(headers, HEADER_TIMESTAMP)?;
    let nonce = required_header(headers, HEADER_NONCE)?;
    let signature = required_header(headers, HEADER_SIGNATURE)?;

    validate_timestamp(timestamp)?;
    validate_nonce(nonce)?;

    let key = load_authorized_key(data_dir, key_fingerprint)?;
    verify_signature(&key, timestamp, nonce, signature, body)?;
    state.accept_nonce(nonce)?;
    Ok(())
}

pub(crate) fn authorized_keys_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MANAGEMENT_AUTHORIZED_KEYS_FILE)
}

fn required_header<'a>(
    headers: &'a HeaderMap,
    name: &'static str,
) -> Result<&'a str, ManagementAuthError> {
    let value = headers
        .get(name)
        .ok_or(ManagementAuthError::Required)?
        .to_str()
        .map_err(|_| ManagementAuthError::Failed)?
        .trim();
    if value.is_empty() {
        return Err(ManagementAuthError::Required);
    }
    Ok(value)
}

fn validate_timestamp(timestamp: &str) -> Result<(), ManagementAuthError> {
    let timestamp = timestamp
        .parse::<i64>()
        .map_err(|_| ManagementAuthError::Failed)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ManagementAuthError::Failed)?
        .as_secs() as i64;

    if (now - timestamp).abs() > AUTH_WINDOW_SECS {
        return Err(ManagementAuthError::Failed);
    }

    Ok(())
}

fn validate_nonce(nonce: &str) -> Result<(), ManagementAuthError> {
    if !(16..=128).contains(&nonce.len()) {
        return Err(ManagementAuthError::Failed);
    }

    if !nonce
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ManagementAuthError::Failed);
    }

    Ok(())
}

fn load_authorized_key(
    data_dir: &Path,
    fingerprint: &str,
) -> Result<PublicKey, ManagementAuthError> {
    let content = std::fs::read_to_string(authorized_keys_path(data_dir))
        .map_err(|_| ManagementAuthError::Required)?;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Ok(key) = line.parse::<PublicKey>() else {
            continue;
        };

        if key.fingerprint(HashAlg::Sha256).to_string() == fingerprint {
            return Ok(key);
        }
    }

    Err(ManagementAuthError::Failed)
}

fn verify_signature(
    key: &PublicKey,
    timestamp: &str,
    nonce: &str,
    signature: &str,
    body: &[u8],
) -> Result<(), ManagementAuthError> {
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .map_err(|_| ManagementAuthError::Failed)?;
    let signature_pem =
        String::from_utf8(signature_bytes).map_err(|_| ManagementAuthError::Failed)?;
    let signature = signature_pem
        .parse::<SshSig>()
        .map_err(|_| ManagementAuthError::Failed)?;
    let message = tako_core::management_auth_message(timestamp, nonce, body);

    key.verify(tako_core::MANAGEMENT_AUTH_NAMESPACE, &message, &signature)
        .map_err(|_| ManagementAuthError::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    fn now_secs() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string()
    }

    fn signed_headers(
        key: &ssh_key::PrivateKey,
        timestamp: &str,
        nonce: &str,
        body: &[u8],
    ) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let message = tako_core::management_auth_message(timestamp, nonce, body);
        let signature = key
            .sign(
                tako_core::MANAGEMENT_AUTH_NAMESPACE,
                HashAlg::Sha512,
                &message,
            )
            .unwrap();
        let signature_pem = signature.to_pem(ssh_key::LineEnding::LF).unwrap();
        let signature_b64 = base64::engine::general_purpose::STANDARD.encode(signature_pem);

        headers.insert(
            HEADER_KEY_FINGERPRINT,
            HeaderValue::from_str(&key.fingerprint(HashAlg::Sha256).to_string()).unwrap(),
        );
        headers.insert(HEADER_TIMESTAMP, HeaderValue::from_str(timestamp).unwrap());
        headers.insert(HEADER_NONCE, HeaderValue::from_str(nonce).unwrap());
        headers.insert(
            HEADER_SIGNATURE,
            HeaderValue::from_str(&signature_b64).unwrap(),
        );
        headers
    }

    fn write_authorized_key(dir: &Path, key: &ssh_key::PrivateKey) {
        let public = key.public_key().to_openssh().unwrap();
        std::fs::write(authorized_keys_path(dir), format!("{public}\n")).unwrap();
    }

    fn test_key() -> ssh_key::PrivateKey {
        ssh_key::PrivateKey::random(&mut ssh_key::rand_core::OsRng, ssh_key::Algorithm::Ed25519)
            .unwrap()
    }

    #[test]
    fn verify_signed_request_accepts_authorized_key() {
        let temp = tempfile::tempdir().unwrap();
        let key = test_key();
        write_authorized_key(temp.path(), &key);
        let body = br#"{"command":"list"}"#;
        let timestamp = now_secs();
        // CodeQL[rust/hard-coded-cryptographic-value]: fixed nonce is a test fixture, not production auth.
        let headers = signed_headers(&key, &timestamp, "nonce123456789012", body);

        verify_signed_request(temp.path(), &ManagementAuthState::default(), &headers, body)
            .unwrap();
    }

    #[test]
    fn verify_signed_request_rejects_replayed_nonce() {
        let temp = tempfile::tempdir().unwrap();
        let key = test_key();
        write_authorized_key(temp.path(), &key);
        let body = br#"{"command":"list"}"#;
        let timestamp = now_secs();
        // CodeQL[rust/hard-coded-cryptographic-value]: fixed nonce is required to test replay rejection.
        let headers = signed_headers(&key, &timestamp, "nonce123456789012", body);
        let state = ManagementAuthState::default();

        verify_signed_request(temp.path(), &state, &headers, body).unwrap();
        let err = verify_signed_request(temp.path(), &state, &headers, body).unwrap_err();

        assert!(matches!(err, ManagementAuthError::Failed));
    }

    #[test]
    fn verify_signed_request_requires_enrolled_key_file() {
        let temp = tempfile::tempdir().unwrap();
        let key = test_key();
        let body = br#"{"command":"list"}"#;
        let timestamp = now_secs();
        // CodeQL[rust/hard-coded-cryptographic-value]: fixed nonce is a test fixture, not production auth.
        let headers = signed_headers(&key, &timestamp, "nonce123456789012", body);

        let err =
            verify_signed_request(temp.path(), &ManagementAuthState::default(), &headers, body)
                .unwrap_err();

        assert!(matches!(err, ManagementAuthError::Required));
    }
}
