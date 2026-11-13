use serde::Serialize;
use serde_json::json;

use crate::api::{RuntimeError, RuntimeResult};
use crate::shared::{
    MAX_SHARED_BYTES, MAX_SHARED_RECORD_BYTES, MAX_SHARED_RECORDS, SecretGrant, SecretRecord,
    SharedSnapshot, SharedStore,
};

use super::{CorrosionStore, parse_query, parse_transaction, record_key, record_payload};

impl SharedStore for CorrosionStore {
    fn shared_snapshot(&mut self) -> RuntimeResult<SharedSnapshot> {
        let mut snapshot = SharedSnapshot::empty(self.network.id.clone());
        let high = parse_query(&self.post("/v1/queries", &json!([
            "SELECT kind, record_id, digest, payload FROM tako_shared_records ORDER BY kind DESC, record_id DESC, digest DESC LIMIT 1", []
        ]))?)?;
        let Some(high) = high.first() else {
            return Ok(snapshot);
        };
        let upper = record_key(high)?;
        let mut cursor = (String::new(), String::new(), String::new());
        let mut bytes = 0;
        loop {
            let page = parse_query(&self.post("/v1/queries", &json!([
                "SELECT kind, record_id, digest, payload FROM tako_shared_records WHERE (kind, record_id, digest) > (?, ?, ?) AND (kind, record_id, digest) <= (?, ?, ?) ORDER BY kind, record_id, digest LIMIT 32",
                [cursor.0, cursor.1, cursor.2, upper.0, upper.1, upper.2]
            ]))?)?;
            if page.is_empty() {
                break;
            }
            if page.len() > 32 {
                return Err(RuntimeError::Corrosion("shared state page size"));
            }
            for row in page {
                let key = record_key(&row)?;
                if key <= cursor || key > upper {
                    return Err(RuntimeError::Corrosion("shared state page order"));
                }
                cursor = key;
                let payload = record_payload(&row)?;
                bytes += payload.len();
                if bytes > MAX_SHARED_BYTES
                    || snapshot.secrets.len() + snapshot.grants.len() >= MAX_SHARED_RECORDS
                {
                    return Err(RuntimeError::Capacity("shared secret state"));
                }
                match cursor.0.as_str() {
                    "secret" => {
                        let record: SecretRecord = serde_json::from_str(payload)?;
                        if record.id.to_string() != cursor.1 {
                            return Err(RuntimeError::Corrosion("secret operation ID"));
                        }
                        snapshot.secrets.push(record);
                    }
                    "grant" => {
                        let grant: SecretGrant = serde_json::from_str(payload)?;
                        if grant.id() != cursor.1 {
                            return Err(RuntimeError::Corrosion("secret grant ID"));
                        }
                        snapshot.grants.push(grant);
                    }
                    _ => return Err(RuntimeError::Corrosion("shared state record kind")),
                }
            }
        }
        snapshot.validate()?;
        Ok(snapshot)
    }

    fn append_secret(&mut self, record: &SecretRecord) -> RuntimeResult<()> {
        let mut snapshot = self.shared_snapshot()?;
        if let Some(previous) = snapshot
            .secrets
            .iter()
            .find(|previous| previous.id == record.id)
        {
            return if previous == record {
                Ok(())
            } else {
                Err(RuntimeError::Conflict("secret operation"))
            };
        }
        snapshot.secrets.push(record.clone());
        snapshot.validate()?;
        self.insert_shared("secret", &record.id.to_string(), record)
    }

    fn append_secret_grant(&mut self, grant: &SecretGrant) -> RuntimeResult<()> {
        let mut snapshot = self.shared_snapshot()?;
        if let Some(previous) = snapshot
            .grants
            .iter()
            .find(|previous| previous.id() == grant.id())
        {
            return if previous == grant {
                Ok(())
            } else {
                Err(RuntimeError::Conflict("secret grant"))
            };
        }
        snapshot.grants.push(grant.clone());
        snapshot.validate()?;
        self.insert_shared("grant", &grant.id(), grant)
    }
}

impl CorrosionStore {
    fn insert_shared(&self, kind: &str, id: &str, record: &impl Serialize) -> RuntimeResult<()> {
        let payload = serde_json::to_string(record)?;
        if payload.len() > MAX_SHARED_RECORD_BYTES {
            return Err(RuntimeError::Capacity("shared record"));
        }
        let digest = hex::encode(openssl::sha::sha256(payload.as_bytes()));
        let bytes = self.post("/v1/transactions", &json!([[
            "INSERT OR IGNORE INTO tako_shared_records (kind, record_id, digest, payload) VALUES (?, ?, ?, ?)",
            [kind, id, &digest, &payload]
        ]]))?;
        parse_transaction(&bytes, 1)?;
        let rows = parse_query(&self.post("/v1/queries", &json!([
            "SELECT kind, record_id, digest, payload FROM tako_shared_records WHERE kind=? AND record_id=? ORDER BY digest LIMIT 2", [kind, id]
        ]))?)?;
        if rows.len() != 1
            || record_key(&rows[0])? != (kind.into(), id.into(), digest)
            || record_payload(&rows[0])? != payload
        {
            return Err(RuntimeError::Conflict("shared record publication"));
        }
        Ok(())
    }
}
