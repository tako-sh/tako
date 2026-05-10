//! Bootstrap envelope passed to every Tako-managed process on fd 3.
//!
//! The SDK reads a JSON object `{"token": ..., "secrets": {...}}` from
//! the inherited read end of a pipe at startup and uses it to populate
//! the `secrets` export on `tako.gen.ts` and the internal auth token used
//! by `Host: <app>.tako` RPCs.
//!
//! This module is the server-side contract. Both spawners — `tako-server`
//! (HTTP app instances) and `tako-workflows` (workflow workers) — must
//! produce the exact same envelope so the SDK's fd-3 parser can't
//! silently drift into a "shape mismatch" crash the next time a spawner
//! is touched.

use std::collections::HashMap;

/// Serialize a Tako bootstrap envelope (`{token, secrets}`) to the JSON
/// bytes that go onto fd 3. Infallible for the concrete input types:
/// `&str` and `HashMap<String, String>` both serialize without error.
pub fn envelope_bytes(token: &str, secrets: &HashMap<String, String>) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "token": token,
        "secrets": secrets,
    }))
    .expect("string/string map always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_bytes_produces_token_and_secrets_object() {
        let secrets =
            HashMap::from([("DATABASE_URL".to_string(), "postgres://host/db".to_string())]);
        let bytes = envelope_bytes("tok-abc", &secrets);
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["token"], "tok-abc");
        assert_eq!(parsed["secrets"]["DATABASE_URL"], "postgres://host/db");
    }

    #[test]
    fn envelope_bytes_with_empty_secrets_still_emits_object() {
        let bytes = envelope_bytes("tok-xyz", &HashMap::new());
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
        let bytes = envelope_bytes("T", &secrets);
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let obj = parsed.as_object().expect("top level must be object");
        assert_eq!(obj.len(), 2, "exactly token + secrets keys");
        assert!(obj.contains_key("token"));
        assert!(obj.contains_key("secrets"));
    }
}
