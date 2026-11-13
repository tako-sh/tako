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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DeviceKind, LocalDeviceKeys, NetworkDescriptor, NetworkId, RequestId,
        shared::{SHARED_SCHEMA, SecretAddress, SecretKind},
    };
    use std::cell::RefCell;
    thread_local! { static DB: RefCell<rusqlite::Connection> = RefCell::new(rusqlite::Connection::open_in_memory().unwrap()); }
    fn transport(path: &str, body: &serde_json::Value) -> RuntimeResult<Vec<u8>> {
        DB.with_borrow(|db| {
            if path == "/v1/transactions" {
                let mut results = Vec::new();
                for statement in body.as_array().unwrap() {
                    let args: Vec<_> = statement[1]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap())
                        .collect();
                    let count = db
                        .execute(
                            statement[0].as_str().unwrap(),
                            rusqlite::params_from_iter(args),
                        )
                        .unwrap();
                    results.push(json!({"rows_affected":count}));
                }
                return Ok(serde_json::to_vec(&json!({"results":results})).unwrap());
            }
            assert_eq!(path, "/v1/queries");
            let args: Vec<_> = body[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let mut statement = db.prepare(body[0].as_str().unwrap()).unwrap();
            let columns = statement
                .column_names()
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let rows = statement
                .query_map(rusqlite::params_from_iter(args), |row| {
                    (0..4)
                        .map(|i| row.get::<_, String>(i))
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap();
            let mut lines = vec![json!({"columns":columns}).to_string()];
            for (index, row) in rows.enumerate() {
                lines.push(json!({"row":[index,row.unwrap()]}).to_string());
            }
            lines.push(json!({"eoq":{}}).to_string());
            Ok(lines.join("\n").into_bytes())
        })
    }
    #[test]
    fn shared_sql_paginates_retries_and_rejects_conflicting_or_corrupt_records() {
        DB.with_borrow(|db| db.execute_batch(SHARED_SCHEMA).unwrap());
        let network = NetworkId::random().unwrap();
        let id = LocalDeviceKeys::generate()
            .unwrap()
            .device_id(&network, DeviceKind::Server)
            .unwrap();
        let mut store = CorrosionStore::new(
            "http://127.0.0.1:9834",
            super::super::BearerToken::new("fixture-token-not-a-credential".into()).unwrap(),
            NetworkDescriptor {
                id: network,
                name: "test".parse().unwrap(),
                initial_admin: id.clone(),
            },
        )
        .unwrap();
        store.transport = Some(transport);
        let mut last = None;
        for revision in 1..=65 {
            let record = SecretRecord {
                id: RequestId::random().unwrap(),
                address: SecretAddress {
                    scope: "app/production".parse().unwrap(),
                    kind: SecretKind::App,
                    name: "TOKEN".into(),
                },
                revision,
                issuer: id.clone(),
                author: id.clone(),
                value: None,
                keys: Default::default(),
            };
            SharedStore::append_secret(&mut store, &record).unwrap();
            last = Some(record);
        }
        let record = last.unwrap();
        SharedStore::append_secret(&mut store, &record).unwrap();
        let snapshot = SharedStore::shared_snapshot(&mut store).unwrap();
        assert_eq!(snapshot.secrets.len(), 65);
        assert_eq!(snapshot.latest().unwrap()[&record.address].revision, 65);
        let mut conflict = record.clone();
        conflict.revision += 1;
        assert!(SharedStore::append_secret(&mut store, &conflict).is_err());
        // Corrosion can merge two immutable payloads for one operation; fail closed.
        let payload = serde_json::to_string(&conflict).unwrap();
        let digest = hex::encode(openssl::sha::sha256(payload.as_bytes()));
        DB.with_borrow(|db| {
            db.execute(
                "INSERT INTO tako_shared_records VALUES ('secret',?,?,?)",
                rusqlite::params![record.id.to_string(), digest, payload],
            )
            .unwrap()
        });
        assert!(SharedStore::shared_snapshot(&mut store).is_err());
        DB.with_borrow(|db| {
            db.execute("UPDATE tako_shared_records SET payload='{}'", [])
                .unwrap()
        });
        assert!(SharedStore::shared_snapshot(&mut store).is_err());
    }
}
