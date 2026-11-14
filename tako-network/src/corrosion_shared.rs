use super::{CorrosionStore, parse_query, parse_transaction, record_key, record_payload};
use crate::api::{RuntimeError, RuntimeResult};
use crate::shared::{
    LegacySecretGrant, MAX_SHARED_RECORD_BYTES, SecretRecord, SharedSnapshot, SharedStore,
};
use serde_json::json;
use std::collections::BTreeSet;

type RowKey = (String, String, String);

impl SharedStore for CorrosionStore {
    fn shared_snapshot(&mut self) -> RuntimeResult<SharedSnapshot> {
        let (snapshot, observed) = self.load_shared()?;
        self.replace_shared(&snapshot, &observed)?;
        Ok(snapshot)
    }

    fn append_secret(&mut self, record: &SecretRecord) -> RuntimeResult<()> {
        let (mut snapshot, observed) = self.load_shared()?;
        let mut addition = SharedSnapshot::empty(self.network.id.clone());
        addition.secrets.push(record.clone());
        snapshot.merge(&addition)?;
        self.replace_shared(&snapshot, &observed)?;
        Ok(())
    }
}

impl CorrosionStore {
    fn load_shared(&self) -> RuntimeResult<(SharedSnapshot, Vec<RowKey>)> {
        let mut snapshot = SharedSnapshot::empty(self.network.id.clone());
        let mut observed = Vec::new();
        let mut seen_ids = BTreeSet::new();
        // Read values first so persisted legacy grants can be folded into their
        // current value. New writers never create separate grant rows.
        for kind in ["secret", "grant"] {
            let high = parse_query(&self.post("/v1/queries", &json!([
                "SELECT kind, record_id, digest, payload FROM tako_shared_records WHERE kind=? ORDER BY record_id DESC, digest DESC LIMIT 1", [kind]
            ]))?)?;
            let Some(high) = high.first() else {
                continue;
            };
            let upper = record_key(high)?;
            let mut cursor = (String::new(), String::new(), String::new());
            loop {
                let page = parse_query(&self.post("/v1/queries", &json!([
                    "SELECT kind, record_id, digest, payload FROM tako_shared_records WHERE kind=? AND (record_id, digest) > (?, ?) AND (record_id, digest) <= (?, ?) ORDER BY record_id, digest LIMIT 32",
                    [kind, cursor.1, cursor.2, upper.1, upper.2]
                ]))?)?;
                if page.is_empty() {
                    break;
                }
                if page.len() > 32 {
                    return Err(RuntimeError::Corrosion("shared state page size"));
                }
                let mut values = SharedSnapshot::empty(self.network.id.clone());
                for row in page {
                    let key = record_key(&row)?;
                    if key.0 != kind || key <= cursor || key > upper {
                        return Err(RuntimeError::Corrosion("shared state page order"));
                    }
                    cursor = key.clone();
                    let payload = record_payload(&row)?;
                    if payload.len() > MAX_SHARED_RECORD_BYTES {
                        return Err(RuntimeError::Capacity("shared record"));
                    }
                    if kind == "secret" {
                        let record: SecretRecord = serde_json::from_str(payload)?;
                        if record.id.to_string() != key.1 {
                            return Err(RuntimeError::Corrosion("secret operation ID"));
                        }
                        seen_ids.insert(record.id.clone());
                        values.secrets.push(record);
                        observed.push(key);
                    } else {
                        let grant: LegacySecretGrant = serde_json::from_str(payload)?;
                        if format!("{}:{}:{}", grant.record, grant.recipient, grant.issuer) != key.1
                        {
                            return Err(RuntimeError::Corrosion("secret grant ID"));
                        }
                        snapshot.import_grant(&grant)?;
                        // An old grant may arrive before its value. Leave that
                        // row intact until the value is seen; never erase the
                        // only key a newly admitted server might have received.
                        if seen_ids.contains(&grant.record) {
                            observed.push(key);
                        }
                    }
                }
                snapshot.merge(&values)?;
            }
        }
        snapshot.validate()?;
        Ok((snapshot, observed))
    }

    fn replace_shared(&self, snapshot: &SharedSnapshot, observed: &[RowKey]) -> RuntimeResult<()> {
        snapshot.validate()?;
        let seen: BTreeSet<_> = observed.iter().collect();
        let mut keep = BTreeSet::new();
        let mut statements = Vec::new();
        for record in &snapshot.secrets {
            let payload = serde_json::to_string(record)?;
            let digest = hex::encode(openssl::sha::sha256(payload.as_bytes()));
            let key = ("secret".to_string(), record.id.to_string(), digest);
            if !seen.contains(&key) {
                statements.push(json!([
                    "INSERT OR IGNORE INTO tako_shared_records (kind, record_id, digest, payload) VALUES (?, ?, ?, ?)",
                    [&key.0, &key.1, &key.2, &payload]
                ]));
            }
            keep.insert(key);
        }
        // Insert current values before removing superseded versions, including
        // during a partial request failure. Delete only rows this read observed:
        // unseen concurrent candidates are resolved by the next reconciliation.
        for key in observed {
            if !keep.contains(key) {
                statements.push(json!([
                    "DELETE FROM tako_shared_records WHERE kind=? AND record_id=? AND digest=?",
                    [&key.0, &key.1, &key.2]
                ]));
            }
        }
        for batch in statements.chunks(32) {
            let bytes = self.post("/v1/transactions", &json!(batch))?;
            parse_transaction(&bytes, batch.len())?;
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
        DB.with_borrow(|db| {
            db.execute_batch("DROP TABLE IF EXISTS tako_shared_records")
                .unwrap();
            db.execute_batch(SHARED_SCHEMA).unwrap();
        });
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
        assert_eq!(snapshot.secrets.len(), 1);
        assert_eq!(snapshot.latest().unwrap()[&record.address].revision, 65);
        DB.with_borrow(|db| {
            assert_eq!(
                db.query_row("SELECT count(*) FROM tako_shared_records", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
        });
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

    #[test]
    fn legacy_rows_are_compacted_without_losing_late_arriving_recipient_keys() {
        DB.with_borrow(|db| {
            db.execute_batch("DROP TABLE IF EXISTS tako_shared_records")
                .unwrap();
            db.execute_batch(SHARED_SCHEMA).unwrap();
        });
        let network = NetworkId::random().unwrap();
        let keys = LocalDeviceKeys::generate().unwrap();
        let id = keys.device_id(&network, DeviceKind::Server).unwrap();
        let mut store = CorrosionStore::new(
            "http://127.0.0.1:9834",
            super::super::BearerToken::new("fixture-token-not-a-credential".into()).unwrap(),
            NetworkDescriptor {
                id: network.clone(),
                name: "test".parse().unwrap(),
                initial_admin: id.clone(),
            },
        )
        .unwrap();
        store.transport = Some(transport);
        let content_key = crate::sealed::random_content_key().unwrap();
        let operation = RequestId::random().unwrap();
        let address = SecretAddress {
            scope: "app/production".parse().unwrap(),
            kind: SecretKind::App,
            name: "TOKEN".into(),
        };
        let context = address.context(&network, &operation).unwrap();
        let wrapped = crate::sealed::SealedValue::seal(
            &keys.encryption_public_key().unwrap(),
            &content_key[..],
            &context,
        )
        .unwrap();
        let record = SecretRecord {
            id: operation,
            address,
            revision: 1,
            issuer: id.clone(),
            author: id.clone(),
            value: Some(
                crate::sealed::Ciphertext::encrypt(&content_key, b"retained", &context).unwrap(),
            ),
            keys: [(id.clone(), wrapped)].into(),
        };
        let recipient = LocalDeviceKeys::generate().unwrap();
        let recipient_id = recipient.device_id(&network, DeviceKind::Laptop).unwrap();
        let recipient_context = serde_json::to_vec(&(
            "tako-shared-secret-key-v1",
            &network,
            &record.address,
            &record.id,
            &recipient_id,
            hex::encode(openssl::sha::sha256(
                &serde_json::to_vec(&record.value).unwrap(),
            )),
        ))
        .unwrap();
        let key = crate::sealed::SealedValue::seal(
            &recipient.encryption_public_key().unwrap(),
            &content_key[..],
            &recipient_context,
        )
        .unwrap();
        let grant =
            json!({"record": record.id, "issuer": id, "recipient": recipient_id, "key": key});
        let insert = |kind: &str, id: String, payload: String| {
            let digest = hex::encode(openssl::sha::sha256(payload.as_bytes()));
            DB.with_borrow(|db| {
                db.execute(
                    "INSERT INTO tako_shared_records VALUES (?,?,?,?)",
                    rusqlite::params![kind, id, digest, payload],
                )
                .unwrap()
            });
        };
        insert(
            "grant",
            format!("{}:{}:{}", record.id, recipient_id, id),
            grant.to_string(),
        );
        assert!(store.shared_snapshot().unwrap().secrets.is_empty());
        DB.with_borrow(|db| {
            assert_eq!(
                db.query_row("SELECT count(*) FROM tako_shared_records", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            )
        });
        insert(
            "secret",
            record.id.to_string(),
            serde_json::to_string(&record).unwrap(),
        );
        // Legacy grants were individually bounded, so a valid value can fold
        // into a record larger than the old 128 KiB per-row limit.
        for _ in 0..800 {
            let extra_keys = LocalDeviceKeys::generate().unwrap();
            let extra_id = extra_keys.device_id(&network, DeviceKind::Laptop).unwrap();
            let context = serde_json::to_vec(&(
                "tako-shared-secret-key-v1",
                &network,
                &record.address,
                &record.id,
                &extra_id,
                hex::encode(openssl::sha::sha256(
                    &serde_json::to_vec(&record.value).unwrap(),
                )),
            ))
            .unwrap();
            let key = crate::sealed::SealedValue::seal(
                &extra_keys.encryption_public_key().unwrap(),
                &content_key[..],
                &context,
            )
            .unwrap();
            insert(
                "grant",
                format!("{}:{}:{}", record.id, extra_id, id),
                json!({"record": record.id, "issuer": id, "recipient": extra_id, "key": key})
                    .to_string(),
            );
        }
        let state = store.shared_snapshot().unwrap();
        assert_eq!(state.secrets[0].keys.len(), 802);
        assert!(serde_json::to_vec(&state.secrets[0]).unwrap().len() > 128 * 1024);
        // Read the newly compacted row back through the Corrosion payload limit.
        assert_eq!(store.shared_snapshot().unwrap(), state);
        assert_eq!(
            &*state.secrets[0]
                .decrypt(&state, &recipient, &recipient_id)
                .unwrap()
                .unwrap(),
            b"retained"
        );
        let previous = serde_json::to_string(&record.value).unwrap();
        let mut deletion = record;
        deletion.id = RequestId::random().unwrap();
        deletion.revision = 2;
        deletion.value = None;
        deletion.keys.clear();
        store.append_secret(&deletion).unwrap();
        let final_state = store.shared_snapshot().unwrap();
        assert_eq!(final_state.secrets, [deletion]);
        DB.with_borrow(|db| {
            let rows: Vec<String> = db
                .prepare("SELECT payload FROM tako_shared_records")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(rows.len(), 1);
            assert!(!rows[0].contains(&previous));
            assert!(!rows[0].contains("ephemeral"));
        });
    }
}
