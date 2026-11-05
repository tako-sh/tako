use crate::{
    RegistrySnapshot,
    api::{RuntimeError, RuntimeResult},
    corrosion::{CORROSION_SCHEMA, MAX_REGISTRY_BYTES},
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::cell::RefCell;

thread_local! {
    static DB: RefCell<Connection> = RefCell::new(Connection::open_in_memory().unwrap());
}

pub fn seed(snapshot: &RegistrySnapshot) {
    DB.with(|db| {
        let mut db = db.borrow_mut();
        *db = Connection::open_in_memory().unwrap();
        db.execute_batch(CORROSION_SCHEMA).unwrap();
        let insert = |kind: &str, id: &str, value: Value| {
            let payload = serde_json::to_string(&value).unwrap();
            db.execute(
                "INSERT INTO tako_network_records VALUES (?1, ?2, ?3, ?4)",
                params![
                    kind,
                    id,
                    hex::encode(openssl::sha::sha256(payload.as_bytes())),
                    payload
                ],
            )
            .unwrap();
        };
        insert(
            "network",
            "network",
            serde_json::to_value(&snapshot.network).unwrap(),
        );
        for member in &snapshot.members {
            insert(
                "member",
                &member.device_id.to_string(),
                serde_json::to_value(member).unwrap(),
            );
        }
        for revocation in &snapshot.revocations {
            insert(
                "revocation",
                &revocation.id.to_string(),
                serde_json::to_value(revocation).unwrap(),
            );
        }
        for ack in &snapshot.acks {
            insert(
                "ack",
                &format!("{}:{}:{}", ack.revocation, ack.server, ack.boot),
                serde_json::to_value(ack).unwrap(),
            );
        }
    });
}

pub fn count() -> i64 {
    DB.with(|db| {
        db.borrow()
            .query_row("SELECT count(*) FROM tako_network_records", [], |r| {
                r.get(0)
            })
            .unwrap()
    })
}

pub fn post(path: &str, body: &Value) -> RuntimeResult<Vec<u8>> {
    let bytes = DB.with(|db| -> RuntimeResult<Vec<u8>> {
        let db = db.borrow();
        match path {
            "/v1/queries" => {
                let mut statement = db.prepare(body[0].as_str().unwrap())?;
                let values: Vec<rusqlite::types::Value> = body[1]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| {
                        if let Some(text) = v.as_str() {
                            rusqlite::types::Value::Text(text.to_owned())
                        } else {
                            rusqlite::types::Value::Integer(v.as_i64().unwrap())
                        }
                    })
                    .collect();
                let rows = statement.query_map(rusqlite::params_from_iter(values), |r| {
                    Ok(vec![
                        r.get::<_, String>(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                    ])
                })?;
                let mut bytes =
                    b"{\"columns\":[\"kind\",\"record_id\",\"digest\",\"payload\"]}\n".to_vec();
                for (i, row) in rows.enumerate() {
                    serde_json::to_writer(&mut bytes, &json!({"row": [i + 1, row?]}))?;
                    bytes.push(b'\n');
                }
                bytes.extend_from_slice(b"{\"eoq\":{}}\n");
                Ok(bytes)
            }
            "/v1/transactions" => {
                let transaction = db.unchecked_transaction()?;
                let mut results = Vec::new();
                for statement in body.as_array().unwrap() {
                    let values: Vec<&str> = statement[1]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap())
                        .collect();
                    let affected = transaction.execute(
                        statement[0].as_str().unwrap(),
                        rusqlite::params_from_iter(values),
                    )?;
                    results.push(json!({"rows_affected": affected}));
                }
                transaction.commit()?;
                Ok(serde_json::to_vec(&json!({"results": results}))?)
            }
            _ => panic!("unexpected adapter path: {path}"),
        }
    })?;
    if bytes.len() > MAX_REGISTRY_BYTES {
        return Err(RuntimeError::Capacity("Corrosion response"));
    }
    Ok(bytes)
}
