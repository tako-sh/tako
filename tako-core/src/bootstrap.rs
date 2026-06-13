//! Bootstrap envelope passed to every Tako-managed process.
//!
//! The SDK reads a JSON object `{"token": ..., "secrets": {...}}` from
//! the inherited read end of a pipe at startup for native processes, or
//! from `TAKO_BOOTSTRAP_DATA` for container processes. It uses that
//! envelope to populate `tako.secrets` from `tako.sh` and the internal
//! auth token used by `Host: <app>.tako` RPCs.
//!
//! This module is the server-side contract. Both spawners — `tako-server`
//! (HTTP app instances) and `tako-workflows` (workflow workers) — must
//! produce the exact same envelope so the SDK's fd-3 parser can't
//! silently drift into a "shape mismatch" crash the next time a spawner
//! is touched.

use std::collections::HashMap;

use crate::storage::StorageBinding;

/// Environment variable used to deliver the bootstrap envelope to container
/// processes. Native app processes receive the same bytes through fd 3.
pub const TAKO_BOOTSTRAP_DATA_ENV: &str = "TAKO_BOOTSTRAP_DATA";

/// Serialize a Tako bootstrap envelope (`{token, secrets, storages}`) to the
/// JSON bytes that go onto fd 3. Infallible for the concrete input types.
pub fn envelope_bytes(
    token: &str,
    secrets: &HashMap<String, String>,
    storages: &HashMap<String, StorageBinding>,
) -> Vec<u8> {
    let envelope = serde_json::json!({
        "token": token,
        "secrets": secrets,
        "storages": storages,
    });
    serde_json::to_vec(&envelope).expect("string/string map always serializes")
}

/// Serialize a Tako bootstrap envelope to a UTF-8 string for transports that
/// carry environment variables instead of bytes.
pub fn envelope_string(
    token: &str,
    secrets: &HashMap<String, String>,
    storages: &HashMap<String, StorageBinding>,
) -> String {
    String::from_utf8(envelope_bytes(token, secrets, storages))
        .expect("JSON envelope is always UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_bytes_produces_token_and_secrets_object() {
        let secrets =
            HashMap::from([("DATABASE_URL".to_string(), "postgres://host/db".to_string())]);
        let bytes = envelope_bytes("tok-abc", &secrets, &HashMap::new());
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["token"], "tok-abc");
        assert_eq!(parsed["secrets"]["DATABASE_URL"], "postgres://host/db");
    }

    #[test]
    fn envelope_bytes_with_empty_secrets_still_emits_object() {
        let bytes = envelope_bytes("tok-xyz", &HashMap::new(), &HashMap::new());
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["token"], "tok-xyz");
        assert!(
            parsed["secrets"].is_object(),
            "secrets must be an object even when empty — the SDK's \
             typeof === 'object' check rejects arrays and null"
        );
        assert_eq!(parsed["secrets"].as_object().unwrap().len(), 0);
    }

    #[test]
    fn envelope_bytes_matches_sdk_contract_exactly() {
        // Regression: workers hung or crashed when the supervisor sent a
        // bare secrets map instead of the {token, secrets} envelope the
        // SDK requires. This test pins the shape so a future refactor
        // can't silently drop the outer object.
        let secrets = HashMap::from([("K".to_string(), "V".to_string())]);
        let bytes = envelope_bytes("T", &secrets, &HashMap::new());
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let obj = parsed.as_object().expect("top level must be object");
        assert_eq!(obj.len(), 3, "exactly token + secrets + storages keys");
        assert!(obj.contains_key("token"));
        assert!(obj.contains_key("secrets"));
        assert!(obj.contains_key("storages"));
    }
}
