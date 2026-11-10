use std::collections::{BTreeMap, HashSet};
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
    expirations: BTreeMap<i64, Vec<String>>,
    set: HashSet<String>,
}

impl ManagementAuthState {
    fn accept_nonce(
        &self,
        nonce: &str,
        now: i64,
        expires_at: i64,
    ) -> Result<(), ManagementAuthError> {
        let mut seen = self.seen.lock();
        while let Some((&expiry, _)) = seen.expirations.first_key_value()
            && expiry < now
        {
            if let Some((_, nonces)) = seen.expirations.pop_first() {
                for nonce in nonces {
                    seen.set.remove(&nonce);
                }
            }
        }

        if seen.set.contains(nonce) {
            return Err(ManagementAuthError::Failed);
        }

        let value = nonce.to_string();
        // A future-dated signature remains valid longer than the window measured
        // from receipt. Retain its nonce through the signed timestamp's expiry.
        seen.expirations
            .entry(expires_at)
            .or_default()
            .push(value.clone());
        seen.set.insert(value);
        Ok(())
    }
}

pub(crate) fn verify_signed_request(
    data_dir: &Path,
    state: &ManagementAuthState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), ManagementAuthError> {
    verify_signed_request_at(data_dir, state, headers, body, current_unix_secs()?)
}

fn verify_signed_request_at(
    data_dir: &Path,
    state: &ManagementAuthState,
    headers: &HeaderMap,
    body: &[u8],
    now: i64,
) -> Result<(), ManagementAuthError> {
    let key_fingerprint = required_header(headers, HEADER_KEY_FINGERPRINT)?;
    let timestamp = required_header(headers, HEADER_TIMESTAMP)?;
    let nonce = required_header(headers, HEADER_NONCE)?;
    let signature = required_header(headers, HEADER_SIGNATURE)?;

    let signed_at = validate_timestamp(timestamp, now)?;
    validate_nonce(nonce)?;

    let key = load_authorized_key(data_dir, key_fingerprint)?;
    verify_signature(&key, timestamp, nonce, signature, body)?;
    state.accept_nonce(nonce, now, signed_at.saturating_add(AUTH_WINDOW_SECS))?;
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

fn current_unix_secs() -> Result<i64, ManagementAuthError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ManagementAuthError::Failed)?
        .as_secs() as i64)
}

fn validate_timestamp(timestamp: &str, now: i64) -> Result<i64, ManagementAuthError> {
    let timestamp = timestamp
        .parse::<i64>()
        .map_err(|_| ManagementAuthError::Failed)?;

    if now.abs_diff(timestamp) > AUTH_WINDOW_SECS as u64 {
        return Err(ManagementAuthError::Failed);
    }

    Ok(timestamp)
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
    fn future_dated_signature_cannot_be_replayed_while_still_valid() {
        let temp = tempfile::tempdir().unwrap();
        let key = test_key();
        write_authorized_key(temp.path(), &key);
        let body = br#"{"command":"list"}"#;
        let headers = signed_headers(&key, "1300", "future_nonce_1234", body);
        let state = ManagementAuthState::default();
        verify_signed_request_at(temp.path(), &state, &headers, body, 1000).unwrap();
        for now in [1301, 1600] {
            assert!(verify_signed_request_at(temp.path(), &state, &headers, body, now).is_err());
        }
    }

    #[test]
    fn extreme_timestamps_are_rejected_without_overflow() {
        for timestamp in [i64::MIN, i64::MAX] {
            assert!(validate_timestamp(&timestamp.to_string(), 1000).is_err());
        }
    }

    #[test]
    fn accept_nonce_rejects_replay_after_many_later_requests() {
        let state = ManagementAuthState::default();
        // CodeQL[rust/hard-coded-cryptographic-value]: fixed nonce is a replay-test fixture, not a production nonce.
        let first_nonce = "nonce00000000001";
        state.accept_nonce(first_nonce, 1_000, 1_300).unwrap();

        for index in 2..=5_000 {
            let nonce = format!("nonce{index:011}");
            state.accept_nonce(&nonce, 1_001, 1_301).unwrap();
        }

        let err = state.accept_nonce(first_nonce, 1_002, 1_300).unwrap_err();
        assert!(matches!(err, ManagementAuthError::Failed));
    }

    #[test]
    fn nonce_expiry_is_ordered_by_signature_time() {
        let state = ManagementAuthState::default();
        state.accept_nonce("future", 1000, 1600).unwrap();
        state.accept_nonce("past", 1000, 1000).unwrap();
        state.accept_nonce("new", 1001, 1301).unwrap();
        let seen = state.seen.lock();
        assert!(!seen.set.contains("past"));
        assert!(seen.set.contains("future"));
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
